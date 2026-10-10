//! The TUI: tasks grouped by status on the left, the selected task on the right. It only reads
//! the state directory; workers run detached, so closing it changes nothing.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
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
use ratatui::layout::{Constraint, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Clear, List, ListItem, ListState, Padding, Paragraph, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Wrap,
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

/// The inbox: each group's statuses in list order, with their labels; `Discarded` isn't shown.
/// Answered questions and failed requests need you too, and planning requests are working.
const GROUPS: [(&str, &[(Status, &str)]); 3] = [
    (
        "Needs you",
        &[
            (Status::Review, "Review"),
            (Status::Failed, "Failed"),
            (Status::Proposed, "Proposed"),
        ],
    ),
    (
        "Working",
        &[(Status::Running, "Running"), (Status::Checking, "Checking")],
    ),
    (
        "Later",
        &[
            (Status::Approved, "Queued"),
            (Status::PrOpen, "PR open"),
            (Status::Merged, "Merged"),
        ],
    ),
];

/// A status's position in list order and its label.
fn status_rank(s: Status) -> Option<(usize, &'static str)> {
    let mut all = GROUPS.iter().flat_map(|g| g.1).enumerate();
    all.find(|(_, (g, _))| *g == s).map(|(i, (_, l))| (i, *l))
}

/// The inbox group a status is in.
fn group_of(s: Status) -> usize {
    GROUPS
        .iter()
        .position(|g| g.1.iter().any(|(g, _)| *g == s))
        .unwrap_or(GROUPS.len() - 1)
}

const TABS: [&str; 6] = ["Summary", "Activity", "Gate", "Findings", "Diff", "Run"];
const FINDINGS: usize = 3;
const DIFF: usize = 4;
const RUN: usize = 5;
/// Lines a scroll key moves the detail pane.
const SCROLL: u16 = 10;

const KEYS: [(&str, &str); 25] = [
    ("n", "new task"),
    ("j/k", "move"),
    ("tab", "pane"),
    ("1-6", "tabs"),
    ("d", "diff"),
    ("m", "open PR"),
    ("M", "merge to main"),
    ("a", "approve"),
    ("A", "approve all"),
    ("e", "edit"),
    ("r", "reply"),
    ("t", "retry"),
    ("p", "plan it"),
    ("y", "copy"),
    ("x", "discard"),
    ("?", "commands"),
    ("q", "quit"),
    ("c", "continue"),
    ("w", "rewind"),
    ("R", "run"),
    ("o", "open"),
    (",", "settings"),
    ("pgup/pgdn", "scroll"),
    ("z", "zoom"),
    ("]", "next for you"),
];

/// How a Settings field changes: cycling through choices, stepping a number within bounds, or
/// flipping a bool.
#[derive(Clone, Copy)]
enum Field {
    Pick(&'static [&'static str]),
    Step(f64, f64, f64),
    Toggle,
}

const MODELS: &[&str] = &[
    "claude-opus-5-5",
    "claude-sonnet-5-5",
    "claude-haiku-5-5",
    "claude-fable-5-1",
];

/// What Settings edits, as (table, key, label, field); an empty label continues the row above.
const SETTINGS: [(&str, &str, &str, Field); 26] = [
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
        "worker",
        "concurrency",
        "concurrency",
        Field::Step(1.0, 1.0, 16.0),
    ),
    ("worker", "slots", "slots", Field::Step(1.0, 1.0, 16.0)),
    (
        "worker",
        "budget_usd",
        "budget usd",
        Field::Step(1.0, 0.0, 100.0),
    ),
    (
        "build",
        "max_cargo",
        "cargo builds",
        Field::Step(1.0, 1.0, 16.0),
    ),
    (
        "critic",
        "max_rounds",
        "fix rounds",
        Field::Step(1.0, 0.0, 9.0),
    ),
    (
        "ask",
        "concurrency",
        "questions at once",
        Field::Step(1.0, 1.0, 8.0),
    ),
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
    (
        "disk",
        "max_target_gb",
        "max target gb",
        Field::Step(10.0, 10.0, 500.0),
    ),
    (
        "disk",
        "min_free_gb",
        "min free gb",
        Field::Step(10.0, 0.0, 1000.0),
    ),
    ("pr", "draft", "draft PRs", Field::Toggle),
    ("tui", "mouse", "mouse", Field::Toggle),
];
/// Each Settings section's first row and heading.
const SECTIONS: [(usize, &str); 5] = [
    (0, "Models"),
    (10, "Workers"),
    (16, "Watch"),
    (22, "Disk"),
    (24, "Other"),
];

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
            (Field::Toggle, toml::Value::Boolean(b)) => (!b).into(),
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

/// Every color and glyph, so a light-terminal or ASCII variant is one swap. Only key and reason
/// chips and the hovered target paint a background: elsewhere the terminal's own theme shows
/// through.
pub struct Theme {
    accent: Color,
    /// Behind a footer key.
    key: Color,
    /// Text on a reason chip.
    ink: Color,
    /// Behind the target under the mouse.
    hover: Color,
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
    /// Before the agent's text in Activity.
    gutter: &'static str,
    /// Before a markdown list item.
    bullet: &'static str,
    /// Before a child task, under its parent.
    tree: &'static str,
    /// Pipeline links up to the current stage and after it, the current stage's mark, a passed
    /// gate check, and a gauge's empty cells (`done` fills it).
    done: &'static str,
    todo: &'static str,
    current: &'static str,
    dot: &'static str,
    rest: &'static str,
    spinner: &'static [&'static str],
    /// A slot meter's held and free cells.
    slot: (&'static str, &'static str),
    /// A group heading's unfolded and folded marks, and its rule.
    fold: (&'static str, &'static str),
    rule: &'static str,
    need: &'static str,
    ascii: bool,
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
            ink: rgb(26, 27, 38, Color::Black),
            hover: rgb(59, 66, 97, Color::DarkGray),
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
            gutter: glyphs("│", "|"),
            bullet: glyphs("•", "-"),
            tree: glyphs("└ ", "- "),
            done: glyphs("━", "="),
            todo: glyphs("┄", "-"),
            current: glyphs("◉", "(*)"),
            dot: glyphs("●", "*"),
            rest: glyphs("─", "-"),
            spinner: if ascii {
                &["|", "/", "-", "\\"]
            } else {
                &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
            },
            slot: if ascii { ("#", ".") } else { ("▰", "▱") },
            fold: if ascii { ("v", ">") } else { ("▾", "▸") },
            rule: glyphs("─", "-"),
            need: glyphs("●", "*"),
            ascii,
        }
    }

    /// ` ready ` on `color`, or `[ready]` in ASCII.
    fn chip(&self, text: &str, color: Color) -> Span<'static> {
        match self.ascii {
            true => Span::styled(format!("[{text}]"), color).bold(),
            false => Span::styled(format!(" {text} "), self.ink).bg(color).bold(),
        }
    }

    /// Green, amber and red mean pass, unsure and fail; the accent is running work.
    fn glyph(&self, status: Status, gate_failed: bool, tick: usize) -> Span<'static> {
        match status {
            Status::Running => Span::styled(self.spinner[tick % self.spinner.len()], self.accent),
            Status::Checking => Span::styled(self.checking, self.accent),
            Status::Review if gate_failed => Span::styled(self.fail, self.red),
            Status::Review | Status::PrOpen | Status::Merged => Span::styled(self.pass, self.green),
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
    /// The command palette's query and selected row, while it's open.
    palette: Option<(String, usize)>,
    compose: Option<Compose>,
    /// An error to show as a toast until `esc` or a click on it.
    notice: Option<String>,
    /// Progress or an outcome to show as a toast for 4 s, or until a click on it.
    info: Option<String>,
    /// The `info` being shown and when it first was, for `expire`.
    info_since: Option<(String, Instant)>,
    /// Push and open the previewed PR once the "pushing" toast has been drawn.
    opening: bool,
    tab: usize,
    /// The selected task's Activity lines, newest last.
    activity: Vec<Act>,
    gate_log: String,
    /// The selected task's changed files as (path, added, deleted).
    diff: Vec<(String, u64, u64)>,
    /// Each of `diff`'s files' hunk lines, and which files the Diff tab shows them for.
    hunks: Vec<Vec<String>>,
    unfolded: BTreeSet<usize>,
    /// Which task and file version `gate_log` and `diff` were read for.
    loaded: Option<(String, Option<SystemTime>)>,
    /// Asking whether to do the selection's `x` (discard) or `M` (merge to main).
    confirm: Option<char>,
    /// A slot, its base and optionally one file, whose diff to page once the TUI is suspended.
    pager: Option<(PathBuf, String, Option<String>)>,
    /// Showing the selected task's PR draft.
    preview: bool,
    /// A task being drafted, with the draft it had and the drafting worker's pid, to preview
    /// once a new one lands; or, when the bool is set, being merged to main.
    awaiting: Option<(String, Option<pr::Draft>, u32, bool)>,
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
    scroll: Scroll,
    /// The selected task's findings, and the cursor over their actionable ones.
    findings: Findings,
    finding: usize,
    /// The actionable findings marked with space for `f`; cleared when the findings reload.
    marked: BTreeSet<usize>,
    /// Tasks in Review with a disputed finding.
    disputed: Vec<String>,
    /// The selected question's id, answer and the repo files the answer cites.
    answer: Option<(String, String, Vec<String>)>,
    run: RunTab,
    /// Tasks whose run script is running.
    serving: Vec<String>,
    /// Each Running task's latest tool call and when its stream last wrote.
    steps: Vec<(String, (String, String), SystemTime)>,
    /// `[watch] stall_after`; a Running row's quiet time turns amber at half of it.
    stall_after: Duration,
    settings: Option<Settings>,
    /// Running in VS Code's terminal.
    vscode: bool,
    /// A `code --wait` on a file from `edit_file`, applied once it exits.
    editing: Option<(Child, Edit)>,
    /// A file and line to open in `$EDITOR` once the TUI is suspended.
    open_at: Option<(PathBuf, Option<u32>)>,
    /// The cursor over the Diff tab's files.
    diff_file: usize,
    /// The cursor file moved or folded, so the Diff tab scrolls it into view once.
    follow: Cell<bool>,
    /// The detail pane has the full width.
    zoom: bool,
    /// The last row click, to spot a double-click.
    last_click: Option<(Position, Instant)>,
    /// The list pane's width in columns, set by dragging the divider.
    split: u16,
    /// The divider is being dragged.
    dragging: bool,
    /// The selected Running task's context in tokens, from its stream's latest message.
    context: Option<u64>,
    /// `[worker] budget_usd`, for a task without its own.
    budget: f64,
    /// `[watch] autocompact` and `handoff_at`, for the context gauge.
    window: (u64, f64),
    /// Which `GROUPS` are folded, for the session.
    folded: [bool; 3],
    /// `[worker] slots`, one cell each in the header's slot meter.
    slots: u32,
    /// Where the last frame drew things, for the mouse.
    hits: RefCell<Hits>,
    /// The mouse's cell, whose target gets a tint and its hint in its pane's bottom border.
    hover: Option<Position>,
    /// Where a right-click opened the selection's actions menu.
    menu: Option<Position>,
}

/// A file `e` edits, as (task id, whether it's the PR draft, path).
type Edit = (String, bool, PathBuf);

/// The list and detail panes, for the wheel, and each clickable rect with its hover hint, in
/// drawing order.
#[derive(Default)]
struct Hits {
    list: Rect,
    detail: Rect,
    /// The detail tabs, where the wheel cycles them.
    tabs: Rect,
    palette: Rect,
    /// The compose panes; a hint outside them, like Submit's, goes in the last one's border.
    panes: Vec<Rect>,
    targets: Vec<(Rect, Target, String)>,
}

impl Hits {
    /// The topmost target at `p`.
    fn at(&self, p: Position) -> Option<&(Rect, Target, String)> {
        self.targets.iter().rev().find(|(r, ..)| r.contains(p))
    }
}

/// What a click does: press a key, or one of the things with no key: select a list row, fold
/// a group, dismiss the info toast, start dragging the divider, scroll the detail pane to
/// an offset, fold or unfold a Diff file, open Diff at a finding's file, select a finding, run
/// a palette row, or pick a compose mode or focus a compose field.
/// `Hint` only shows its hint on hover.
#[derive(Clone, Copy)]
enum Target {
    Hint,
    Key(KeyEvent),
    Row(usize),
    Info,
    Divider,
    Scroll(u16),
    Fold(usize),
    File(usize),
    Location(usize),
    Finding(usize),
    Command(usize),
    Mode(usize),
    Focus(usize),
}

/// The detail pane's scroll offset, and the scrollbar its tab asked for this frame as (area,
/// largest offset, whether the offset counts up from the tail).
#[derive(Default)]
struct Scroll {
    offset: Cell<u16>,
    bar: Cell<Option<(Rect, u16, bool)>>,
}

impl Scroll {
    fn get(&self) -> u16 {
        self.offset.get()
    }

    fn set(&self, offset: u16) {
        self.offset.set(offset);
    }
}

/// The key a footer label stands for; pairs like `j/k` stand for none.
fn key_of(label: &str) -> Option<KeyEvent> {
    let code = match label {
        "enter" => KeyCode::Enter,
        "esc" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "space" => KeyCode::Char(' '),
        "ctrl-s" => return Some(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
        _ => match label.chars().collect::<Vec<_>>()[..] {
            [c] => KeyCode::Char(c),
            _ => return None,
        },
    };
    Some(KeyEvent::new(code, KeyModifiers::NONE))
}

/// The digit key for the `i`th detail tab.
fn digit(i: usize) -> KeyEvent {
    KeyEvent::new(KeyCode::Char((b'1' + i as u8) as char), KeyModifiers::NONE)
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
    let cfg = config::load(repo).ok();
    let mut app = App {
        repo: repo.to_path_buf(),
        state: state.clone(),
        name: name.into_owned(),
        requests: Vec::new(),
        tasks: Vec::new(),
        selected: 0,
        detail: false,
        palette: None,
        compose: None,
        notice: None,
        info: None,
        info_since: None,
        opening: false,
        tab: 0,
        activity: Vec::new(),
        gate_log: String::new(),
        diff: Vec::new(),
        hunks: Vec::new(),
        unfolded: BTreeSet::new(),
        loaded: None,
        confirm: None,
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
        scroll: Scroll::default(),
        findings: Findings::default(),
        finding: 0,
        marked: BTreeSet::new(),
        disputed: Vec::new(),
        sessions: config::load(repo).map_or(0, |c| c.watch.max_handoffs + 1),
        run: RunTab::default(),
        serving: Vec::new(),
        steps: Vec::new(),
        stall_after: config::load(repo).map_or(Duration::MAX, |c| c.watch.stall_after),
        settings: None,
        vscode: vscode(),
        editing: None,
        open_at: None,
        diff_file: 0,
        follow: Cell::new(false),
        zoom: false,
        last_click: None,
        split: 45,
        dragging: false,
        context: None,
        budget: cfg.as_ref().map_or(0.0, |c| c.worker.budget_usd),
        folded: [false; 3],
        slots: cfg.as_ref().map_or(0, |c| c.worker.slots),
        window: cfg.map_or((0, 1.0), |c| (c.watch.autocompact, c.watch.handoff_at)),
        hits: RefCell::default(),
        hover: None,
        menu: None,
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
            app.expire(Instant::now());
            // compact draws no headings to unfold a group with
            if terminal.size()?.height < COMPACT {
                app.folded = [false; 3];
            }
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
            let until = Instant::now() + wait;
            while event::poll(until.saturating_duration_since(Instant::now()))? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press && !app.key(key) => {
                        return Ok(());
                    }
                    // a move that keeps the same hovered target doesn't redraw
                    Event::Mouse(m) if m.kind == MouseEventKind::Moved => {
                        if !app.hover(Position::new(m.column, m.row)) {
                            continue;
                        }
                    }
                    Event::Mouse(m) if !app.mouse(m) => return Ok(()),
                    _ => {}
                }
                break;
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
        let rank = |s| status_rank(s).map(|r| r.0);
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
        // the selected row is never hidden in a folded group
        if let Some(g) = self
            .groups()
            .iter()
            .position(|g| g.contains(&self.selected))
        {
            self.folded[g] = false;
        }
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
        let step = |id: &String, dir: PathBuf| {
            let log = state.join(format!("logs/{id}.jsonl"));
            let quiet = fs::metadata(&log).and_then(|m| m.modified()).ok()?;
            let call = activity(&log, &dir).into_iter().rfind(|a| a.tool != TEXT)?;
            Some((id.clone(), (call.tool, call.target), quiet))
        };
        let tasks = (self.tasks.iter())
            .filter(|(t, _)| t.status == Status::Running)
            .filter_map(|(t, _)| {
                let slot = t.slot.map(|n| state.join("slots").join(n.to_string()));
                step(&t.id, slot.unwrap_or_default())
            });
        // a lead works in the repo
        let leads = (self.requests.iter())
            .filter(|r| r.status == Phase::Planning)
            .filter_map(|r| step(&r.id, self.repo.clone()));
        self.steps = tasks.chain(leads).collect();
        Ok(())
    }

    /// Whether `j/k`, `r` and `x` act on the Findings tab's findings, whichever pane has focus.
    fn on_findings(&self) -> bool {
        self.tab == FINDINGS && self.task().is_some()
    }

    /// Whether `j/k`, `enter` and `o` act on the Diff tab's files.
    fn on_diff(&self) -> bool {
        self.detail && self.tab == DIFF && self.task().is_some()
    }

    /// Each of `GROUPS`' rows in list order, as indexes into requests then tasks; a group's
    /// requests come after its tasks.
    fn groups(&self) -> [Vec<usize>; 3] {
        let mut groups: [Vec<usize>; 3] = Default::default();
        let n = self.requests.len();
        for (i, (t, _)) in self.tasks.iter().enumerate() {
            groups[group_of(t.status)].push(n + i);
        }
        for (i, r) in self.requests.iter().enumerate() {
            groups[usize::from(r.status == Phase::Planning)].push(i);
        }
        groups
    }

    /// Moves the selection to the next shown row down, or up; folded groups are skipped.
    fn move_by(&mut self, down: bool) {
        let groups = self.groups().into_iter().zip(self.folded);
        let rows: Vec<_> = groups.filter(|g| !g.1).flat_map(|g| g.0).collect();
        let at = rows.iter().position(|&r| r == self.selected);
        let i = match (at, down) {
            (None, _) => 0,
            (Some(i), true) => (i + 1).min(rows.len() - 1),
            (Some(i), false) => i.saturating_sub(1),
        };
        if let Some(&r) = rows.get(i) {
            self.selected = r;
        }
    }

    /// `]`: selects the next Needs you row, wrapping round, and unfolds the group.
    fn next_need(&mut self) {
        let need = &self.groups()[0];
        let at = need.iter().position(|&r| r == self.selected);
        if let Some(&r) = need.get(at.map_or(0, |i| (i + 1) % need.len())) {
            (self.selected, self.folded[0]) = (r, false);
        }
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

    /// Clears `info` 4 s after it first showed.
    fn expire(&mut self, now: Instant) {
        match (&self.info, &self.info_since) {
            (Some(i), Some((shown, at))) if i == shown => {
                if now.duration_since(*at) >= Duration::from_secs(4) {
                    (self.info, self.info_since) = (None, None);
                }
            }
            (Some(i), _) => self.info_since = Some((i.clone(), now)),
            (None, _) => self.info_since = None,
        }
    }

    /// Returns false to quit.
    fn key(&mut self, key: KeyEvent) -> bool {
        self.press(key, false)
    }

    /// `action` runs the key as the task action the menu and palette label it, so the Findings
    /// and Diff tabs' own `x`, `r`, `o`, `j/k` and `enter` don't take it.
    fn press(&mut self, key: KeyEvent, action: bool) -> bool {
        let ctrl =
            |c| key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char(c);
        if ctrl('c') {
            return false;
        }
        // any key closes the actions menu, and all but esc then act as usual
        if self.menu.take().is_some() && key.code == KeyCode::Esc {
            return true;
        }
        if key.code == KeyCode::Esc && self.notice.take().is_some() {
            return true;
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
        if let Some(action) = self.confirm.take() {
            let res = match (key.code, action) {
                (KeyCode::Char('y'), 'M') => self.merge(),
                (KeyCode::Char('y'), _) => self.discard(),
                _ => Ok(()),
            };
            if let Err(e) = res {
                self.notice = Some(format!("{e:#}"));
            }
            return true;
        }
        if self.palette.is_some() {
            let n = self.commands().len();
            let Some((query, row)) = &mut self.palette else {
                return true;
            };
            match key.code {
                KeyCode::Esc => self.palette = None,
                KeyCode::Enter => {
                    let row = *row;
                    return self.command(row);
                }
                KeyCode::Down => *row = (*row + 1).min(n.saturating_sub(1)),
                KeyCode::Up => *row = row.saturating_sub(1),
                KeyCode::Backspace => {
                    query.pop();
                    *row = 0;
                }
                KeyCode::Char(c) => {
                    query.push(c);
                    *row = 0;
                }
                _ => {}
            }
            return true;
        }
        if key.code == KeyCode::Char('q') {
            return false;
        }
        let proposed = self
            .task()
            .is_some_and(|(t, _)| t.status == Status::Proposed);
        let failed = self.request().is_some_and(|r| r.status == Phase::Failed);
        let status = self.task().map(|(t, _)| t.status);
        let answered = self.request().is_some_and(|r| r.status == Phase::Done);
        if !action && self.on_findings() {
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
                KeyCode::Char(' ') if n > 0 => {
                    if !self.marked.remove(&self.finding) {
                        self.marked.insert(self.finding);
                    }
                    return true;
                }
                KeyCode::Char(c @ ('f' | 'F')) => {
                    if let Err(e) = self.send_findings(c == 'F') {
                        self.notice = Some(format!("{e:#}"));
                    }
                    return true;
                }
                _ => {}
            }
        }
        if !action && self.on_diff() {
            match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    let n = self.diff.len();
                    self.diff_file = (self.diff_file + 1).min(n.saturating_sub(1));
                    self.follow.set(true);
                    return true;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.diff_file = self.diff_file.saturating_sub(1);
                    self.follow.set(true);
                    return true;
                }
                KeyCode::Enter => {
                    if !self.unfolded.remove(&self.diff_file) {
                        self.unfolded.insert(self.diff_file);
                    }
                    self.follow.set(true);
                    return true;
                }
                KeyCode::Char('o') => {
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
            KeyCode::Char('?' | ':') => self.palette = Some((String::new(), 0)),
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
            KeyCode::Right => self.tab = (self.tab + 1) % TABS.len(),
            KeyCode::Left => self.tab = (self.tab + TABS.len() - 1) % TABS.len(),
            KeyCode::Char('z') => self.zoom = !self.zoom,
            KeyCode::Esc => self.zoom = false,
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
            KeyCode::Char('x') => {
                self.confirm = (self.task().is_some() || failed || answered).then_some('x');
            }
            KeyCode::Char('M') if valid(self, "M") => self.confirm = Some('M'),
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
            KeyCode::Down | KeyCode::Char('j') => self.move_by(true),
            KeyCode::Up | KeyCode::Char('k') => self.move_by(false),
            KeyCode::Char(']') => self.next_need(),
            KeyCode::Tab => self.detail = !self.detail,
            KeyCode::PageDown | KeyCode::PageUp => self.scroll_by(key.code == KeyCode::PageDown),
            _ => {}
        }
        if (self.selected, self.tab) != before {
            self.scroll.set(0);
            self.diff_file = 0;
        }
        if self.selected != before.0 {
            self.unfolded.clear();
        }
        true
    }
}

impl App {
    /// Scrolls the detail pane a step toward the end of its text (`down`) or back; Activity and
    /// the Gate output and a planning request's lead count from their tail, so down there means
    /// newer.
    fn scroll_by(&mut self, down: bool) {
        let planning = self.request().is_some_and(|r| r.status == Phase::Planning);
        let tail = planning || self.task().is_some() && matches!(self.tab, 1 | 2 | RUN);
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
        if let Some(r) = self.request().filter(|r| r.status == Phase::Planning) {
            let log = self.state.join(format!("logs/{}.jsonl", r.id));
            self.activity = activity(&log, &self.repo);
        }
        let Some((id, since)) = self.task().map(|(t, since)| (t.id.clone(), *since)) else {
            return;
        };
        let log = self.state.join(format!("logs/{id}.jsonl"));
        if self.tab == 1 {
            self.activity = activity(&log, &self.slot_dir().unwrap_or_default());
        }
        if self
            .task()
            .is_some_and(|(t, _)| t.status == Status::Running)
        {
            self.context = context(&log);
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
        // the verdict shows the gate, findings and diffstat on every tab
        if self.loaded != key {
            let log = self.state.join(format!("logs/{id}.gate.log"));
            self.gate_log = fs::read_to_string(log).unwrap_or_default();
            let base = self.base().unwrap_or_default();
            (self.diff, self.hunks) = self
                .slot_dir()
                .map(|d| (diffstat(&d, &base), hunks(&d, &base)))
                .unwrap_or_default();
            self.diff_file = self.diff_file.min(self.diff.len().saturating_sub(1));
            self.findings = Findings::load(&self.state, &id).unwrap_or_default();
            let n = self.findings.actionable().count();
            self.finding = self.finding.min(n.saturating_sub(1));
            self.marked.clear();
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
        let row = SETTINGS
            .iter()
            .position(|r| (r.0, r.1) == ("worker", "concurrency"));
        let concurrency = row.is_some_and(|i| s.values[i] != s.loaded[i]);
        let path = s.save(&self.repo, &config::home()?)?;
        let watch = config::load(&self.repo)?.watch;
        (self.sessions, self.stall_after) = (watch.max_handoffs + 1, watch.stall_after);
        // a raised limit starts queued tasks now rather than on the next worker exit
        if concurrency {
            sched::run(&self.repo)?;
        }
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
        self.awaiting = Some((t.id.clone(), t.pr_draft.clone(), pid, false));
        self.preview = false;
        Ok(())
    }

    /// `M` once confirmed: has a worker rebase, re-gate and push the selected task to main.
    fn merge(&mut self) -> Result<()> {
        ensure!(self.awaiting.is_none(), "a PR draft or merge is on its way");
        let (t, _) = self.task().context("no task selected")?;
        let id = t.id.clone();
        let _ = fs::remove_file(self.state.join(format!("logs/{id}.pr.log"))); // may not exist
        let pid = worker::spawn(&self.repo, &id, &["--merge"])?;
        self.awaiting = Some((id, None, pid, true));
        Ok(())
    }

    /// Opens the preview once the awaited task has a new draft; gives up if it can't get one
    /// or its worker `exited` without one.
    fn check_awaiting(&mut self, exited: bool) {
        let Some((id, old, _, merge)) = &self.awaiting else {
            return;
        };
        // never move the selection under an open modal, whose keys would then act on it
        let modal = self.confirm.is_some() || self.palette.is_some() || self.reply.is_some();
        let modal = modal || self.compose.is_some();
        if modal || self.instruction.is_some() {
            return;
        }
        let Some(i) = self.tasks.iter().position(|(t, _)| &t.id == id) else {
            self.awaiting = None; // discarded meanwhile
            return;
        };
        let t = &self.tasks[i].0;
        if *merge {
            // it may pass through Checking or Running on the way; one left in Review has its
            // reason in the log
            if t.status != Status::Merged && !exited {
                return;
            }
            if t.status == Status::Review {
                let log = self.state.join(format!("logs/{id}.pr.log"));
                let why = fs::read_to_string(log).unwrap_or("the worker exited".into());
                self.notice = Some(format!("not merged: {}", why.trim()));
            }
            self.awaiting = None;
        } else if t.status == Status::Review && t.pr_draft.is_some() && t.pr_draft != *old {
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

    /// `o` on the Diff tab: the selected file's diff in VS Code's diff editor against its
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

    /// The palette's rows matching its query by subsequence: the selection's actions, the other
    /// keys valid here, then `Go to` each task, as (key, label, context, what it runs).
    fn commands(&self) -> Vec<(&'static str, String, String, Target)> {
        let query = self.palette.as_ref().map_or("", |p| &p.0).to_lowercase();
        let here = match (self.task(), self.request()) {
            (Some((t, _)), _) => t.title.clone(),
            (_, Some(r)) => r.text.lines().next().unwrap_or_default().to_string(),
            _ => String::new(),
        };
        let actions = actions(self);
        let others = KEYS
            .into_iter()
            .filter(|(k, _)| *k != "?" && valid(self, k) && !actions.iter().any(|(a, _)| a == k));
        let keys = (actions.iter().map(|&(k, l)| (k, l, here.clone())))
            .chain(others.map(|(k, l)| (k, l, "anywhere".to_string())))
            .filter_map(|(k, l, at)| Some((k, l.to_string(), at, Target::Key(key_of(k)?))));
        let n = self.requests.len();
        let go = self.tasks.iter().enumerate().map(|(i, (t, _))| {
            let status = status_rank(t.status).map_or("", |(_, l)| l);
            let label = format!("Go to {}", t.title);
            ("", label, status.to_string(), Target::Row(n + i))
        });
        let matches = |(k, l, ..): &(&str, String, String, Target)| {
            let hay = format!("{k} {l}").to_lowercase();
            let mut hay = hay.chars();
            query.chars().all(|c| hay.any(|h| h == c))
        };
        keys.chain(go).filter(matches).collect()
    }

    /// Closes the palette and runs its `i`th row. Returns false to quit.
    fn command(&mut self, i: usize) -> bool {
        let commands = self.commands();
        let target = commands
            .get(i.min(commands.len().saturating_sub(1)))
            .map(|c| c.3);
        self.palette = None;
        match target {
            Some(Target::Key(key)) => return self.press(key, true),
            Some(Target::Row(i)) if i != self.selected => {
                self.selected = i;
                self.scroll.set(0);
                self.diff_file = 0;
                self.unfolded.clear();
            }
            _ => {}
        }
        true
    }

    /// Moves the hover to `at`; true when that changes the hovered target, which needs a redraw.
    /// Hovering a palette row selects it.
    fn hover(&mut self, at: Position) -> bool {
        let hits = self.hits.borrow();
        let rect = |p: Option<Position>| p.and_then(|p| hits.at(p)).map(|(r, ..)| *r);
        let changed = rect(self.hover) != rect(Some(at));
        if let (Some((_, row)), Some((_, Target::Command(i), _))) = (&mut self.palette, hits.at(at))
        {
            *row = *i;
        }
        drop(hits);
        self.hover = Some(at);
        changed
    }

    /// A click presses the key of the target under it or selects its row; the wheel scrolls the
    /// detail pane or moves through the list. Returns false to quit.
    fn mouse(&mut self, m: MouseEvent) -> bool {
        let modal = self.compose.is_some() || self.settings.is_some() || self.reply.is_some();
        let modal = modal
            || self.preview
            || self.confirm.is_some()
            || self.palette.is_some()
            || self.instruction.is_some();
        let at = Position::new(m.column, m.row);
        let (in_list, in_detail, in_tabs, in_palette, hit) = {
            let hits = self.hits.borrow();
            (
                hits.list.contains(at),
                hits.detail.contains(at),
                hits.tabs.contains(at),
                hits.palette.contains(at),
                hits.at(at).map(|(_, t, _)| *t),
            )
        };
        // a click on an actions menu item presses its key; any click closes the menu
        if self.menu.is_some() {
            if let MouseEventKind::Down(_) = m.kind {
                self.menu = None;
                if let Some(Target::Key(key)) = hit {
                    self.last_click = Some((at, Instant::now()));
                    return self.press(key, true);
                }
            }
            return true;
        }
        let before = self.selected;
        match (m.kind, hit) {
            (MouseEventKind::Down(MouseButton::Right), Some(Target::Row(i))) if !modal => {
                self.selected = i;
                self.menu = Some(at);
            }
            (MouseEventKind::Drag(MouseButton::Left), _) if self.dragging => {
                self.split = (m.column + 1).clamp(30, 70);
            }
            (MouseEventKind::Up(MouseButton::Left), _) => self.dragging = false,
            (MouseEventKind::Down(MouseButton::Left), Some(Target::Command(i))) => {
                self.last_click = Some((at, Instant::now()));
                return self.command(i);
            }
            (MouseEventKind::Down(_), _) if self.palette.is_some() && !in_palette => {
                self.palette = None;
            }
            // the second half of a double-click that opened the confirm doesn't answer it
            (MouseEventKind::Down(MouseButton::Left), Some(Target::Key(_)))
                if self.confirm.is_some()
                    && self.last_click.is_some_and(|(p, t)| {
                        p == at && t.elapsed() < Duration::from_millis(400)
                    }) => {}
            (MouseEventKind::Down(MouseButton::Left), Some(Target::Key(key))) => {
                return self.key(key);
            }
            (MouseEventKind::Down(MouseButton::Left), Some(Target::Divider)) if !modal => {
                self.dragging = true;
            }
            (MouseEventKind::Down(MouseButton::Left), Some(Target::Scroll(offset))) if !modal => {
                self.scroll.set(offset);
            }
            (MouseEventKind::Down(MouseButton::Left), Some(Target::Row(i))) if !modal => {
                // crossterm doesn't report double-clicks: two Downs on one cell within 400 ms
                let now = Instant::now();
                let double = i == self.selected
                    && self.last_click.is_some_and(|(p, t)| {
                        p == at && now.duration_since(t) < Duration::from_millis(400)
                    });
                self.zoom |= double;
                self.last_click = (!double).then_some((at, now));
                self.selected = i;
            }
            (MouseEventKind::Down(MouseButton::Left), Some(Target::Info)) => self.info = None,
            (MouseEventKind::ScrollDown | MouseEventKind::ScrollUp, _) if !modal => {
                let down = m.kind == MouseEventKind::ScrollDown;
                if in_tabs {
                    let n = TABS.len();
                    return self.key(digit((self.tab + if down { 1 } else { n - 1 }) % n));
                } else if in_detail {
                    self.scroll_by(down);
                } else if in_list {
                    self.move_by(down);
                }
            }
            (MouseEventKind::Down(MouseButton::Left), Some(Target::Fold(g))) if !modal => {
                self.folded[g] = !self.folded[g];
                if self.folded[g] && self.groups()[g].contains(&self.selected) {
                    self.move_by(true);
                }
            }
            (MouseEventKind::Down(MouseButton::Left), Some(Target::File(i))) if !modal => {
                (self.detail, self.diff_file) = (true, i);
                if !self.unfolded.remove(&i) {
                    self.unfolded.insert(i);
                }
            }
            (MouseEventKind::Down(MouseButton::Left), Some(Target::Location(i))) if !modal => {
                (self.detail, self.tab, self.diff_file) = (true, DIFF, i);
                self.unfolded = BTreeSet::from([i]);
                // with only file `i` unfolded its row is `i`, so this puts it at the top
                self.scroll.set(i as u16);
            }
            (MouseEventKind::Down(MouseButton::Left), Some(Target::Finding(i))) if !modal => {
                self.finding = i;
            }
            (MouseEventKind::Down(MouseButton::Left), Some(Target::Mode(i))) => {
                if let Some(c) = &mut self.compose {
                    (c.mode, c.focus) = (MODES[i].0, 2);
                }
            }
            (MouseEventKind::Down(MouseButton::Left), Some(Target::Focus(i))) => {
                if let Some(c) = &mut self.compose {
                    c.focus = i;
                }
            }
            _ => {}
        }
        if self.selected != before {
            self.scroll.set(0);
            self.diff_file = 0;
            self.unfolded.clear();
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

    /// `f`/`F`: sends the marked findings, else the selected one, or with `all` every actionable
    /// one, back to the worker to fix.
    fn send_findings(&mut self, all: bool) -> Result<()> {
        let (t, _) = self.task().context("no task selected")?;
        ensure!(
            t.status == Status::Review,
            "act on findings once the task is in Review"
        );
        let id = t.id.clone();
        let mut findings = Findings::load(&self.state, &id)?;
        let picked: Vec<usize> = match all {
            true => (0..findings.actionable().count()).collect(),
            false if self.marked.is_empty() => vec![self.finding],
            false => self.marked.iter().copied().collect(),
        };
        let sent = findings.send_back(&picked)?;
        findings.save(&self.state, &id)?;
        // reloading clears the marks, whose indices are stale now
        self.loaded = None;
        worker::spawn(&self.repo, &id, &["--reply", &send_prompt(&sent)])?;
        self.info = Some(format!("sent {} back to the worker", sent.len()));
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

/// The Activity tab's tool name for the agent's text.
const TEXT: &str = "│";

/// An Activity line: a tool call, the agent's text, a nudge or a handoff.
#[derive(Debug, Default, PartialEq)]
struct Act {
    tool: String,
    target: String,
    /// Lines added and removed, for an `Edit`.
    edit: Option<(usize, usize)>,
    /// Whether the call's result wasn't an error, once it has one.
    ok: Option<bool>,
    /// A failed shell call's exit code.
    exit: Option<i64>,
}

/// Tool calls with their results, the agent's text, nudges and handoffs from the tail of a
/// Claude stream log, with `slot/` paths made relative.
fn activity(log: &Path, slot: &Path) -> Vec<Act> {
    // ponytail: only the last 256 KiB, so redrawing a long session stays cheap
    let bytes = tail(log, 256 << 10);
    let prefix = format!("{}/", slot.display());
    let text = String::from_utf8_lossy(&bytes);
    let events = text
        .lines()
        .filter_map(|l| serde_json::from_str::<stream::Event>(l).ok());
    let note = |tool: &str, target: String| Act {
        tool: tool.into(),
        target,
        ..Default::default()
    };
    let (mut acts, mut calls) = (Vec::new(), std::collections::HashMap::new());
    for e in events {
        match e {
            stream::Event::Assistant { message } => {
                for c in message.content {
                    match c {
                        Content::Text { text } => acts.push(note(TEXT, text)),
                        Content::ToolUse { id, name, input } => {
                            let key = match name.as_str() {
                                "Bash" => "command",
                                "Grep" | "Glob" => "pattern",
                                "WebSearch" => "query",
                                "WebFetch" => "url",
                                _ => "file_path",
                            };
                            let target = input[key].as_str().unwrap_or("").lines().next();
                            let target = target.unwrap_or("").replace(&prefix, "");
                            let lines =
                                |k: &str| input[k].as_str().map_or(0, |s| s.lines().count());
                            let edit = (name == "Edit")
                                .then(|| (lines("new_string"), lines("old_string")));
                            calls.insert(id, acts.len());
                            acts.push(Act {
                                edit,
                                ..note(&name, target)
                            });
                        }
                        Content::Other => {}
                    }
                }
            }
            stream::Event::User { message } => {
                for r in message["content"].as_array().into_iter().flatten() {
                    let id = r["tool_use_id"].as_str();
                    let Some(act) = id.and_then(|id| calls.get(id)).map(|&i| &mut acts[i]) else {
                        continue;
                    };
                    let failed = r["is_error"].as_bool().unwrap_or(false);
                    act.ok = Some(!failed);
                    // Claude Code starts a failed Bash result with `Exit code N`
                    act.exit = (r["content"].as_str())
                        .and_then(|s| s.strip_prefix("Exit code "))
                        .and_then(|s| s.split_whitespace().next()?.parse().ok())
                        .filter(|_| failed);
                }
            }
            stream::Event::Nudge { reason } => acts.push(note(NUDGE, reason)),
            stream::Event::Handoff { reason } => acts.push(note(HANDOFF, reason)),
            _ => {}
        }
    }
    acts
}

/// Tokens in context at the latest assistant message in the tail of a Claude stream log.
fn context(log: &Path) -> Option<u64> {
    let bytes = tail(log, 256 << 10);
    let text = String::from_utf8_lossy(&bytes);
    text.lines()
        .rev()
        .find_map(|l| match serde_json::from_str(l).ok()? {
            stream::Event::Assistant { message } => Some(message.usage.context()),
            _ => None,
        })
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
fn scrolled(f: &mut Frame, area: Rect, p: Paragraph, scroll: &Scroll) {
    let max = p
        .line_count(area.width)
        .saturating_sub(area.height as usize);
    scroll.set(scroll.get().min(max as u16));
    if max > 0 {
        scroll.bar.set(Some((area, max as u16, false)));
    }
    f.render_widget(p.scroll((scroll.get(), 0)), area);
}

/// For a view that follows its tail: how many of `len` lines to hide below, clamped so a full
/// page of `area` stays in view.
fn from_tail(len: usize, area: Rect, scroll: &Scroll) -> usize {
    let max = len.saturating_sub(area.height as usize);
    scroll.set(scroll.get().min(max as u16));
    if max > 0 {
        scroll.bar.set(Some((area, max as u16, true)));
    }
    scroll.get() as usize
}

/// A thumb over the pane border column `track`, for top offsets `0..=max` at `top`; returns the
/// top offset each of its rows jumps to.
fn scrollbar(f: &mut Frame, track: Rect, max: usize, top: usize) -> Vec<(Rect, usize)> {
    let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None)
        .track_symbol(None)
        .thumb_symbol("┃");
    let mut state = ScrollbarState::new(max + 1)
        .position(top)
        .viewport_content_length(track.height as usize);
    f.render_stateful_widget(bar, track, &mut state);
    let last = track.height.saturating_sub(1).max(1) as usize;
    track
        .rows()
        .enumerate()
        .map(|(i, row)| (row, (i * max + last / 2) / last))
        .collect()
}

/// `git diff --numstat` against the base, as (path, added, deleted); binaries count 0.
fn diffstat(slot: &Path, base: &str) -> Vec<(String, u64, u64)> {
    let range = format!("{base}...HEAD");
    let out = git(slot, &["diff", "--numstat", "--no-renames", &range]).unwrap_or_default();
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

/// Each file's lines from its first `@@` in `git diff` against the base, in `diffstat`'s order.
fn hunks(slot: &Path, base: &str) -> Vec<Vec<String>> {
    let out = git(slot, &["diff", "--no-renames", &format!("{base}...HEAD")]).unwrap_or_default();
    let out = format!("\n{out}");
    // a type change (file to symlink) is two sections under one header but one numstat line
    let mut files: Vec<(&str, Vec<String>)> = Vec::new();
    for f in out.split("\ndiff --git ").skip(1) {
        let head = f.lines().next().unwrap_or_default();
        let lines = f.lines().skip_while(|l| !l.starts_with("@@"));
        let lines = lines.map(|l| l.replace('\t', "    "));
        match files.last_mut() {
            Some((h, l)) if *h == head => l.extend(lines),
            _ => files.push((head, lines.collect())),
        }
    }
    files.into_iter().map(|(_, l)| l).collect()
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
    if !compact {
        f.render_widget(header_line(app, theme, header, false), header);
    }
    let selected = app.task().map(|(t, _)| t);
    let draft = selected.and_then(|t| t.pr_draft.as_ref());
    if let Some(c) = &app.compose {
        compose(f, body, c, theme, &mut app.hits.borrow_mut());
    } else if let Some(s) = &app.settings {
        settings(f, body, s, theme);
    } else if app.preview
        && let Some(d) = draft
    {
        preview(f, body, d, app.instruction.as_ref(), theme);
    } else if app.zoom {
        detail(f, body, app, theme, true, tick, now);
    } else if body.width >= 100 {
        let [left, right] =
            Layout::horizontal([Constraint::Length(app.split), Constraint::Fill(1)]).areas(body);
        // the two border columns between the panes; row targets drawn later win on the list's
        let divider = Rect::new(
            left.right() - 1,
            body.y + 1,
            2,
            body.height.saturating_sub(2),
        );
        app.hits
            .borrow_mut()
            .targets
            .push((divider, Target::Divider, "drag · resize".into()));
        list(f, left, app, theme, !app.detail, tick, now);
        detail(f, right, app, theme, app.detail, tick, now);
    } else if app.detail {
        detail(f, body, app, theme, true, tick, now);
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
    } else if app.instruction.is_some() {
        vec![("enter", "redraft"), ("esc", "cancel")]
    } else if app.preview {
        let push = ("enter", "push and open PR");
        vec![push, ("g", "regenerate"), ("e", "edit"), ("esc", "back")]
    } else if app.on_findings() {
        // space, f and F first, so they're the last a narrow footer drops
        let mut keys = vec![("j/k", "finding")];
        let any = app.findings.actionable().count() > 0;
        if any {
            keys.extend([("space", "mark"), ("f", "fix"), ("F", "fix all")]);
        }
        let disputed = app.findings.is_disputed(app.finding);
        keys.extend(disputed.then_some(("r", "reply to worker")));
        keys.extend(any.then_some(("x", "waive")));
        keys.extend([
            ("o", "open"),
            ("tab", "pane"),
            ("1-6", "tabs"),
            ("?", "more"),
        ]);
        keys
    } else if app.on_diff() {
        let keys = [
            ("j/k", "file"),
            ("enter", "fold"),
            ("o", "file diff"),
            ("d", "full diff"),
        ];
        [
            &keys[..],
            &[("tab", "pane"), ("1-6", "tabs"), ("?", "more")],
        ]
        .concat()
    } else {
        // up to six keys that do something for the selection, the one that moves it on first
        let global = ["n", "1-6", "j/k", "tab", "q"]
            .into_iter()
            .filter(|k| valid(app, k));
        let mut keys = actions(app);
        keys.extend(global.filter_map(|k| KEYS.into_iter().find(|(key, _)| *key == k)));
        keys.truncate(6);
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
        header_line(app, theme, footer, true).spans
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
    // still too wide: drop the target from labels like `reply to worker`
    if more && width(&keys) > footer.width as usize {
        for (_, label) in &mut keys {
            *label = label.split(' ').next().unwrap_or_default();
        }
    }
    let mut line = match app.editing {
        Some(_) => {
            vec![Span::styled(
                " editing in VS Code; close its tab to apply",
                theme.accent,
            )]
        }
        None => {
            let mut x = footer.x + Line::from(prefix.clone()).width() as u16;
            for k in &keys {
                let w = Line::from(chip(k).to_vec()).width() as u16;
                let rect = Rect::new(x, footer.y, w, 1).intersection(footer);
                let hint = format!("{} · {}", k.0, k.1);
                let target = key_of(k.0).map(|key| (rect, Target::Key(key), hint));
                app.hits.borrow_mut().targets.extend(target);
                x = x.saturating_add(w);
            }
            keys.iter().flat_map(chip).collect()
        }
    };
    line.splice(0..0, prefix);
    f.render_widget(Line::from(line), footer);
    if let Some(at) = app.menu {
        // the selection's actions at the cursor; only its items respond
        app.hits.borrow_mut().targets.clear();
        let mut items = actions(app);
        let own = items.len();
        items.extend([("z", "zoom"), ("]", "next for you")]);
        let all = f.area();
        let (w, h) = (28.min(all.width), items.len() as u16 + 3);
        let x = (at.x + 1).min(all.right().saturating_sub(w));
        let y = (at.y + 1).min(all.bottom().saturating_sub(h));
        let area = Rect::new(x, y, w, h).intersection(all);
        let title = match (app.request(), selected) {
            (Some(r), _) => r.text.lines().next().unwrap_or_default(),
            (None, Some(t)) => t.title.as_str(),
            _ => "",
        };
        let block = pane(title, true, theme);
        let inner = block.inner(area);
        f.render_widget(Clear, area);
        f.render_widget(block, area);
        for (i, (key, label)) in items.into_iter().enumerate() {
            // a rule between the task's keys and the global ones
            let row = inner.y + i as u16 + u16::from(i >= own);
            // discard is red; it still goes through the confirm
            let color = if key == "x" { theme.red } else { theme.accent };
            let line = Line::from(vec![
                Span::styled(format!("{key:<4}"), color).bold(),
                Span::raw(label).fg(if key == "x" { theme.red } else { Color::Reset }),
            ]);
            let rect =
                Rect::new(area.x + 1, row, area.width.saturating_sub(2), 1).intersection(all);
            let text = Rect {
                y: row,
                height: 1,
                ..inner
            }
            .intersection(all);
            f.render_widget(line, text);
            let hint = format!("{key} · {label}");
            let target = key_of(key).map(|k| (rect, Target::Key(k), hint));
            app.hits.borrow_mut().targets.extend(target);
        }
        let rule = Rect {
            y: inner.y + own as u16,
            height: 1,
            ..inner
        }
        .intersection(all);
        f.render_widget(
            Line::raw(theme.rule.repeat(inner.width as usize)).dim(),
            rule,
        );
    }
    if app.palette.is_some() || app.confirm.is_some() || app.reply.is_some() {
        // only an open modal's own targets respond
        app.hits.borrow_mut().targets.clear();
    }
    palette(f, app, theme);
    let confirm = match (app.confirm, app.request(), selected) {
        (None, ..) => None,
        (Some('M'), None, Some(t)) => Some((
            "Merge",
            t.title.as_str(),
            "rebase, re-gate and push it straight to main, with no PR",
            "Cancel",
        )),
        (_, Some(r), _) => Some((
            "Dismiss",
            r.text.lines().next().unwrap_or_default(),
            "dismiss it",
            "Keep it",
        )),
        (_, None, Some(t)) => Some((
            "Discard",
            t.title.as_str(),
            "discard and free its slot",
            "Keep it",
        )),
        _ => None,
    };
    if let Some((verb, title, does, keep)) = confirm {
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
        for (key, label, code) in [("y", verb, KeyCode::Char('y')), ("esc", keep, KeyCode::Esc)] {
            let button = [
                Span::styled(format!(" {key} "), theme.accent)
                    .bold()
                    .bg(theme.key),
                Span::raw(format!(" {label} ")).bg(theme.key),
                Span::raw("   "),
            ];
            let w = Line::from(button[..2].to_vec()).width() as u16;
            let rect = Rect::new(x, buttons.y, w, 1).intersection(buttons);
            let hint = format!("{key} · {label}");
            let key = KeyEvent::new(code, KeyModifiers::NONE);
            app.hits
                .borrow_mut()
                .targets
                .push((rect, Target::Key(key), hint));
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
    // toasts over the bottom right of the body, the info above the error
    let mut bottom = body.bottom().saturating_sub(1);
    for (text, err) in [(&app.notice, true), (&app.info, false)] {
        let Some(text) = text else { continue };
        let color = if err { theme.red } else { theme.accent };
        let w = (Line::raw(text).width() as u16 + 4).min(64.min(body.width.saturating_sub(4)));
        let para = Paragraph::new(text.as_str()).wrap(Wrap { trim: true });
        let para = if err { para.fg(theme.red) } else { para };
        let h = para.line_count(w.saturating_sub(4)) as u16 + 2;
        let x = body.right().saturating_sub(w + 2);
        let area = Rect::new(x, bottom.saturating_sub(h), w, h).intersection(body);
        bottom = area.y;
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(color)
            .padding(Padding::horizontal(1));
        f.render_widget(Clear, area);
        f.render_widget(para.block(block), area);
        let (target, hint) = match err {
            true => (
                Target::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
                "esc",
            ),
            false => (Target::Info, "click"),
        };
        let hint = format!("{hint} · dismiss");
        app.hits.borrow_mut().targets.push((area, target, hint));
    }
    // the hovered target gets a tint, and its hint in the bottom border of its pane
    let hits = app.hits.borrow();
    if let Some(at) = app.hover
        && let Some((rect, _, hint)) = hits.at(at)
    {
        f.buffer_mut()
            .set_style(*rect, Style::new().bg(theme.hover));
        let pane = [hits.list, hits.detail]
            .into_iter()
            .chain(hits.panes.iter().copied())
            .find(|p| p.contains(at))
            .or(hits.panes.last().copied());
        let hint = Line::styled(format!(" {hint} "), theme.accent).right_aligned();
        let area = pane.unwrap_or(body).inner(Margin::new(1, 0));
        f.render_widget(Block::new().title_bottom(hint), area);
    }
}

fn gate_passed(t: &Task) -> bool {
    t.gate.as_ref().is_some_and(|g| g.iter().all(|c| c.passed))
}

/// `f`/`F`'s reply to the worker, listing the findings sent back.
fn send_prompt(sent: &[crate::critic::Finding]) -> String {
    let mut prompt =
        "The human sends these findings back, so fix each one and commit.\n".to_string();
    for (i, f) in sent.iter().enumerate() {
        prompt.push_str(&format!(
            "\n{}. [{:?}] {} - {}\n   Evidence: {}\n",
            i + 1,
            f.severity,
            f.location,
            f.claim,
            f.evidence
        ));
    }
    prompt
}

/// Whether the `KEYS` key `k` does something for the selection.
fn valid(app: &App, k: &str) -> bool {
    let selected = app.task().map(|(t, _)| t);
    let failed = app.request().is_some_and(|r| r.status == Phase::Failed);
    let answered = app.request().is_some_and(|r| r.status == Phase::Done);
    match k {
        "d" | "o" => selected.is_some_and(|t| t.slot.is_some()),
        "x" => selected.is_some() || failed || answered,
        "t" => failed || selected.is_some_and(|t| t.status == Status::Failed),
        "c" => selected.is_some_and(|t| matches!(t.status, Status::Review | Status::Failed)),
        "w" => selected.is_some_and(|t| t.status == Status::Review),
        "p" | "y" => answered,
        "1-6" => selected.is_some(),
        "R" => selected.is_some_and(|t| t.status == Status::Review || app.serving.contains(&t.id)),
        "m" => selected.is_some_and(|t| t.status == Status::Review && gate_passed(t)),
        "M" => selected.is_some_and(|t| {
            let merged = |p: &String| {
                (app.tasks.iter()).any(|(o, _)| &o.id == p && o.status == Status::Merged)
            };
            t.status == Status::Review && gate_passed(t) && t.parent.as_ref().is_none_or(merged)
        }),
        "r" => {
            answered
                || selected.is_some_and(|t| matches!(t.status, Status::Proposed | Status::Review))
        }
        "a" | "e" => selected.is_some_and(|t| t.status == Status::Proposed),
        "A" => app.tasks.iter().any(|(t, _)| t.status == Status::Proposed),
        _ => true,
    }
}

/// The selection's valid keys as (key, label), the one that moves it on first.
fn actions(app: &App) -> Vec<(&'static str, &'static str)> {
    let status = app.task().map(|(t, _)| t.status);
    let answered = app.request().is_some_and(|r| r.status == Phase::Done);
    let first: &[&str] = match status {
        Some(Status::Review) => &["m", "M", "r", "d", "x", "c", "w", "R", "o"],
        Some(Status::Failed) => &["t", "c", "x", "d", "o"],
        Some(Status::Proposed) => &["a", "A", "e", "r", "x"],
        _ if answered => &["p", "r", "y", "x"],
        _ => &["t", "d", "o", "R", "x"],
    };
    first
        .iter()
        .filter(|k| valid(app, k))
        .filter_map(|k| KEYS.into_iter().find(|(key, _)| key == k))
        .map(|(k, label)| match (k, status) {
            ("x", Some(_)) => (k, "discard task"),
            ("r", Some(Status::Review)) => (k, "reply to worker"),
            ("r", _) => (k, "reply to lead"),
            _ => (k, label),
        })
        .collect()
}

/// The command palette: the query, then the matching rows with their key chips and context.
fn palette(f: &mut Frame, app: &App, theme: &Theme) {
    let Some((query, row)) = &app.palette else {
        return;
    };
    let rows = app.commands();
    let all = f.area();
    let n = rows
        .len()
        .clamp(1, all.height.saturating_sub(9).max(1) as usize);
    let area = centered(all, 68.min(all.width.saturating_sub(4)), n as u16 + 4);
    let block = pane("Commands", true, theme);
    let [input, rule, list] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(block.inner(area));
    f.render_widget(Clear, area);
    f.render_widget(block, area);
    let prompt = Line::from(vec![
        Span::styled("› ", theme.accent).bold(),
        Span::raw(query.clone()).bold(),
        Span::styled("▏", theme.accent),
    ]);
    f.render_widget(prompt, input);
    f.render_widget(
        Line::raw("type to filter · ↑↓ · enter")
            .dim()
            .right_aligned(),
        input,
    );
    f.render_widget(
        Line::raw(theme.rule.repeat(rule.width as usize)).dim(),
        rule,
    );
    let mut hits = app.hits.borrow_mut();
    hits.palette = area;
    if rows.is_empty() {
        f.render_widget(Line::raw("No command matches").dim(), list);
    }
    let sel = (*row).min(rows.len().saturating_sub(1));
    let off = sel.saturating_sub(n - 1);
    for (r, (key, label, context, _)) in rows.iter().enumerate().skip(off).take(n) {
        let at = Rect::new(list.x, list.y + (r - off) as u16, list.width, 1);
        let chip = match *key {
            "" => Span::raw(""),
            k => Span::styled(format!(" {k} "), theme.accent)
                .bold()
                .bg(theme.key),
        };
        let context = truncate(context, 22, theme.ellipsis);
        let room = (list.width as usize).saturating_sub(8 + context.width());
        let label = truncate(label, room, theme.ellipsis);
        let pad = 6usize.saturating_sub(chip.width());
        let gap = room.saturating_sub(label.width()) + 1;
        let style = if r == sel {
            Style::new().bold()
        } else {
            Style::new()
        };
        let line = Line::from(vec![
            Span::styled(if r == sel { theme.bar } else { " " }, theme.accent),
            chip,
            Span::raw(" ".repeat(pad)),
            Span::styled(label.clone(), style),
            Span::raw(" ".repeat(gap)),
            Span::raw(context).dim(),
        ]);
        f.render_widget(line, at);
        let hint = match *key {
            "" => label,
            k => format!("{k} · {label}"),
        };
        hits.targets.push((at, Target::Command(r), hint));
    }
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

/// Each role's model and effort, then one row per limit or threshold, by section; `•` marks an unsaved change.
fn settings(f: &mut Frame, area: Rect, s: &Settings, theme: &Theme) {
    let file = if s.global { "global" } else { "project" };
    let mut lines = vec![Line::raw(s.path.clone()).dim()];
    let mut selected = 0;
    for (i, ((_, key, label, _), v)) in SETTINGS.iter().zip(&s.values).enumerate() {
        if let Some((_, heading)) = SECTIONS.iter().find(|(first, _)| *first == i) {
            lines.extend([Line::raw(""), Line::raw(*heading).dim()]);
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
        let name = match i < SECTIONS[1].0 {
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

fn compose(f: &mut Frame, area: Rect, c: &Compose, theme: &Theme, hits: &mut Hits) {
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
    let rect = Rect::new(submit.x, submit.y, button.width() as u16, 1).intersection(submit);
    let target = key_of("ctrl-s").map(|key| (rect, Target::Key(key), "ctrl-s · submit".into()));
    hits.targets.extend(target);
    f.render_widget(Line::from(button), submit);
    let block = pane("Mode", c.focus == 2, theme);
    let inner = block.inner(mode);
    let mut modes = Vec::new();
    for (i, (m, name)) in MODES.iter().enumerate() {
        let span = match *m == c.mode {
            true => Span::styled(*name, theme.accent).bold().underlined(),
            false => Span::raw(*name).dim(),
        };
        let x = inner.x + Line::from(modes.clone()).width() as u16;
        let rect = Rect::new(x, inner.y, span.width() as u16, 1).intersection(inner);
        hits.targets
            .push((rect, Target::Mode(i), format!("click · {name} mode")));
        modes.extend([span, Span::raw("  ")]);
    }
    f.render_widget(Line::from(modes), inner);
    f.render_widget(block, mode);
    for (i, (field, area, title)) in [
        (&c.request, request, "Request"),
        (&c.ticket, ticket, "Ticket"),
    ]
    .into_iter()
    .enumerate()
    {
        let block = pane(title, c.focus == i, theme);
        f.render_widget(field, block.inner(area));
        f.render_widget(block, area);
        let hint = format!("click · type the {}", title.to_lowercase());
        hits.targets.push((area, Target::Focus(i), hint));
    }
    hits.panes = vec![mode, request, ticket];
}

/// ` yogan · repo  ● 2 need you  ⠋ 1 working  ○ 1 queued`, with the slot meter and spend flush
/// right; `compact` keeps ` yogan  ● 2 `. The chip presses `]` and a slot selects its task.
fn header_line(app: &App, theme: &Theme, area: Rect, compact: bool) -> Line<'static> {
    let groups = app.groups();
    let need = groups[0].len();
    let mut spans = vec![Span::styled(" yogan", theme.accent).bold()];
    if !compact {
        spans.push(Span::raw(format!(" · {}", app.name)).dim());
    }
    spans.push(Span::raw("  "));
    let x = area.x + Line::from(spans.clone()).width() as u16;
    let chip = match (need, compact) {
        (0, _) => Span::styled(format!("{} nothing needs you", theme.pass), theme.green),
        (n, true) => theme.chip(&format!("{} {n}", theme.need), theme.amber),
        (n, false) => theme.chip(&format!("{} {n} need you", theme.need), theme.amber),
    };
    let rect = Rect::new(x, area.y, chip.width() as u16, 1).intersection(area);
    let key = KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE);
    let hint = "] · next needing you".into();
    let typing = app.compose.is_some() || app.settings.is_some() || app.instruction.is_some();
    if !typing && !app.preview {
        app.hits
            .borrow_mut()
            .targets
            .push((rect, Target::Key(key), hint));
    }
    spans.push(chip);
    if compact {
        spans.push(Span::raw("  "));
        return Line::from(spans);
    }
    let queued = app
        .tasks
        .iter()
        .filter(|(t, _)| t.status == Status::Approved);
    spans.extend([
        Span::raw("  "),
        theme.glyph(Status::Running, false, 0),
        Span::raw(format!(" {} working  ", groups[1].len())),
        Span::raw(format!("{} {} queued", theme.queued, queued.count())).dim(),
    ]);
    let spent = app.tasks.iter().fold(0.0, |n, (t, _)| n + t.spent());
    let spend = format!("   ${spent:.2} ");
    let meter = 6 + app.slots as usize + spend.width();
    let left = Line::from(spans.clone()).width();
    if left + meter > area.width as usize {
        return Line::from(spans);
    }
    let pad = area.width as usize - left - meter;
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::raw("slots ").dim());
    let mut x = area.x.saturating_add((left + pad + 6) as u16);
    let n = app.requests.len();
    for slot in 1..=app.slots {
        let held = app.tasks.iter().position(|(t, _)| t.slot == Some(slot));
        let rect = Rect::new(x, area.y, 1, 1).intersection(area);
        let mut hits = app.hits.borrow_mut();
        spans.push(match held {
            Some(i) => {
                let t = &app.tasks[i].0;
                let gate_failed = t.gate.iter().flatten().any(|c| !c.passed);
                let hint = format!("slot {slot} · {}", t.title);
                hits.targets.push((rect, Target::Row(n + i), hint));
                Span::styled(theme.slot.0, theme.glyph(t.status, gate_failed, 0).style)
            }
            None => {
                hits.targets
                    .push((rect, Target::Hint, format!("slot {slot} · free")));
                Span::raw(theme.slot.1).dim()
            }
        });
        x = x.saturating_add(1);
    }
    spans.push(Span::raw(spend));
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
    let (mut items, mut selected) = (Vec::new(), None);
    // what a click on each item does, for the mouse; None for blanks
    let mut rows = Vec::new();
    for (g, group) in app.groups().into_iter().enumerate() {
        if group.is_empty() {
            continue;
        }
        if !compact {
            if !items.is_empty() {
                items.push(ListItem::new(""));
                rows.push(None);
            }
            let fold = if app.folded[g] {
                theme.fold.1
            } else {
                theme.fold.0
            };
            let name = format!("{fold} {} ", GROUPS[g].0.to_uppercase());
            let count = format!("{} ", group.len());
            let rule = theme
                .rule
                .repeat(width.saturating_sub(name.width() + count.width()));
            let name = match g {
                0 => Span::styled(name, theme.amber).bold(),
                _ => Span::raw(name).dim().bold(),
            };
            let line = Line::from(vec![name, Span::raw(count).dim(), Span::raw(rule).dim()]);
            items.push(ListItem::new(line));
            rows.push(Some(Target::Fold(g)));
        }
        if app.folded[g] {
            continue;
        }
        for i in group {
            let sel = i == app.selected;
            if sel {
                selected = Some(items.len());
            }
            rows.push(Some(Target::Row(i)));
            let Some((t, since)) = i.checked_sub(app.requests.len()).map(|i| &app.tasks[i]) else {
                let r = &app.requests[i];
                let (glyph, right) = match r.status {
                    Phase::Failed => (
                        Span::styled(theme.fail, theme.red),
                        vec![theme.chip("failed", theme.red)],
                    ),
                    Phase::Done => (
                        Span::styled(theme.pass, theme.green),
                        vec![theme.chip("answered", theme.amber)],
                    ),
                    _ => {
                        let spin = theme.spinner[tick % theme.spinner.len()];
                        let doing = match r.mode {
                            Mode::Ask => "answering",
                            _ => "planning",
                        };
                        (
                            Span::styled(spin, theme.accent),
                            vec![Span::raw(doing).dim()],
                        )
                    }
                };
                let title = r.text.lines().next().unwrap_or_default();
                items.push(ListItem::new(row_line(
                    glyph, title, right, sel, width, theme,
                )));
                if let Some((_, (tool, target), at)) = app.steps.iter().find(|s| s.0 == r.id) {
                    let quiet = now.duration_since(*at).unwrap_or_default();
                    let late = quiet > app.stall_after / 2;
                    items.push(ListItem::new(step(tool, target, quiet, late, width, theme)));
                    rows.push(Some(Target::Row(i)));
                }
                continue;
            };
            let age = since.and_then(|s| now.duration_since(s).ok());
            let age = age.map(short).unwrap_or_default();
            let serving = app.serving.contains(&t.id).then_some("serving · ");
            let path = lineage(t, &app.tasks);
            let gate_failed = t.gate.iter().flatten().any(|c| !c.passed);
            let reason = match t.status {
                Status::Review if app.disputed.contains(&t.id) => {
                    Some(("disputed".into(), theme.amber))
                }
                Status::Review | Status::Failed if gate_failed => {
                    Some(("gate failed".into(), theme.red))
                }
                Status::Review => Some(("ready".into(), theme.green)),
                Status::Failed => Some(("failed".into(), theme.red)),
                // a tree's root counts its proposals; its children read as the tree
                Status::Proposed if path.len() == 1 => {
                    let tree = app.tasks.iter().filter(|(o, _)| {
                        o.status == Status::Proposed && lineage(o, &app.tasks)[0] == t.id
                    });
                    Some((format!("{} to approve", tree.count()), theme.amber))
                }
                _ => None,
            };
            let dim = match t.status {
                Status::Running => format!("working · {age}"),
                Status::Checking => format!("gate · {age}"),
                Status::Approved => "needs a slot".into(),
                Status::PrOpen => match t.pr_url.as_deref().and_then(|u| u.rsplit('/').next()) {
                    Some(n) => format!("PR #{n}"),
                    None => "PR open".into(),
                },
                Status::Merged => "Merged".into(),
                _ => format!("{}{age}", serving.unwrap_or_default()),
            };
            let mut right: Vec<_> = reason.map(|(r, c)| theme.chip(&r, c)).into_iter().collect();
            let gap = if right.is_empty() || dim.is_empty() {
                ""
            } else {
                " "
            };
            right.push(Span::raw(format!("{gap}{dim}")).dim());
            items.push(ListItem::new(row(
                t,
                path.len() - 1,
                right,
                sel,
                width,
                theme,
                tick,
            )));
            if let Some((_, (tool, target), at)) = app.steps.iter().find(|s| s.0 == t.id) {
                let quiet = now.duration_since(*at).unwrap_or_default();
                let late = quiet > app.stall_after / 2;
                items.push(ListItem::new(step(tool, target, quiet, late, width, theme)));
                rows.push(Some(Target::Row(i)));
            }
        }
    }
    let len = items.len();
    let mut state = ListState::default().with_selected(selected);
    f.render_stateful_widget(List::new(items).block(block), area, &mut state);
    // a click on the scrollbar selects the first row at that point of the list
    let max = len.saturating_sub(inner.height as usize);
    let bar = match max {
        0 => Vec::new(),
        _ => {
            let track = Rect::new(area.right() - 1, inner.y, 1, inner.height);
            scrollbar(f, track, max, state.offset())
        }
    };
    let bar: Vec<_> = (bar.into_iter())
        .filter_map(|(r, top)| {
            let mut below = rows[top..].iter().flatten();
            let row = *below.find(|t| matches!(t, Target::Row(_)))?;
            Some((r, row, "click · scroll".into()))
        })
        .collect();
    let mut hits = app.hits.borrow_mut();
    hits.list = area;
    let shown = rows
        .into_iter()
        .skip(state.offset())
        .zip(inner.y..inner.bottom());
    let rows = shown.filter_map(|(target, y)| {
        let target = target?;
        let hint = match target {
            Target::Fold(_) => "click · fold",
            _ => "click select · double-click zoom",
        };
        let rect = Rect {
            y,
            height: 1,
            ..inner
        };
        Some((rect, target, hint.into()))
    });
    hits.targets.extend(rows);
    hits.targets.extend(bar);
}

/// `      edit src/retry.rs…  quiet 2m`, dim under a Running row; `quiet` is amber when `late`.
fn step(
    tool: &str,
    target: &str,
    quiet: Duration,
    late: bool,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let quiet = format!("quiet {}", short(quiet));
    let left = format!("{} {target}", verb(tool, theme).0);
    let left = truncate(
        &left,
        width.saturating_sub(7 + quiet.width()),
        theme.ellipsis,
    );
    let pad = width.saturating_sub(6 + left.width() + quiet.width());
    let quiet = match late {
        true => Span::styled(quiet, theme.amber),
        false => Span::raw(quiet).dim(),
    };
    Line::from(vec![
        Span::raw(format!("      {left}{}", " ".repeat(pad))).dim(),
        quiet,
    ])
}

/// `▌ ⠋ title…          working · 4m`: never wraps, the title gives way.
fn row(
    t: &Task,
    depth: usize,
    right: Vec<Span<'static>>,
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

/// A list row from its parts: selection bar, glyph, title and right-hand spans.
fn row_line(
    glyph: Span<'static>,
    title: &str,
    right: Vec<Span<'static>>,
    sel: bool,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let right_width = right.iter().map(Span::width).sum::<usize>();
    let title = truncate(title, width.saturating_sub(5 + right_width), theme.ellipsis);
    let pad = width.saturating_sub(4 + title.width() + right_width);
    let bar = if sel { theme.bar } else { " " };
    let mut line = Line::from(vec![
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
    ]);
    line.spans.extend(right);
    line
}

fn detail(
    f: &mut Frame,
    area: Rect,
    app: &App,
    theme: &Theme,
    focused: bool,
    tick: usize,
    now: SystemTime,
) {
    app.hits.borrow_mut().detail = area;
    let compact = f.area().height < COMPACT;
    let (label, key, hint) = match app.zoom {
        true => (" esc unzoom ", KeyCode::Esc, "esc · unzoom"),
        false => (" z zoom ", KeyCode::Char('z'), "z · zoom"),
    };
    let block = pane("Task", focused, theme).title(Line::raw(label).dim().right_aligned());
    let inner = block.inner(area);
    f.render_widget(block, area);
    let w = label.width() as u16;
    let rect = Rect::new(area.right().saturating_sub(w + 1), area.y, w, 1).intersection(area);
    let target = Target::Key(KeyEvent::new(key, KeyModifiers::NONE));
    app.hits
        .borrow_mut()
        .targets
        .push((rect, target, hint.into()));
    app.scroll.bar.set(None);
    detail_body(f, inner, app, theme, compact, tick, now);
    // the tab's scrollbar sits on the pane's right border; a click there jumps to that offset
    if let Some((rect, max, tail)) = app.scroll.bar.take() {
        let track = Rect::new(area.right() - 1, rect.y, 1, rect.height);
        let top = if tail {
            max - app.scroll.get()
        } else {
            app.scroll.get()
        };
        let rows = scrollbar(f, track, max as usize, top as usize);
        let rows = rows.into_iter().map(|(r, top)| {
            let offset = if tail { max - top as u16 } else { top as u16 };
            (r, Target::Scroll(offset), "click · scroll".into())
        });
        app.hits.borrow_mut().targets.extend(rows);
    }
}

/// The detail pane's tabs and the active tab, or the selected request.
fn detail_body(
    f: &mut Frame,
    inner: Rect,
    app: &App,
    theme: &Theme,
    compact: bool,
    tick: usize,
    now: SystemTime,
) {
    let Some((t, _)) = app.task() else {
        match app.request() {
            Some(r) => {
                let spinner = theme.spinner[tick % theme.spinner.len()];
                request(f, inner, r, app, spinner, theme)
            }
            None => f.render_widget(Line::raw("No tasks yet.").dim(), inner),
        }
        return;
    };
    let spinner = theme.spinner[tick % theme.spinner.len()];
    // the spend cell gets a row of its own, so its gauge fits
    let mut cells = verdict(app, t, theme, now);
    let spend = cells.pop().unwrap_or_default();
    let mut rows: Vec<_> = cells.chunks(2).map(<[_]>::to_vec).collect();
    rows.push(vec![spend]);
    let gap = Constraint::Length(if compact { 0 } else { 1 });
    let shown = if compact { 1 } else { rows.len() as u16 };
    let [strip, _, verdict_area, _, next, _, tabs, _, body] = Layout::vertical([
        Constraint::Length(1),
        gap,
        Constraint::Length(shown),
        gap,
        Constraint::Length(1),
        gap,
        Constraint::Length(1),
        gap,
        Constraint::Fill(1),
    ])
    .areas(inner);
    f.render_widget(pipeline(t, theme, spinner), strip);
    let cell = |(label, value): &(&str, Vec<Span<'static>>), pad: usize| {
        let label = match label.is_empty() {
            true => String::new(),
            false => format!("{label:<pad$}"),
        };
        [vec![Span::raw(label).dim()], value.clone()].concat()
    };
    if compact {
        let sep = Span::raw("  ·  ").dim();
        let spans = rows.concat().into_iter().enumerate().flat_map(|(i, c)| {
            let lead = (i > 0).then(|| sep.clone());
            lead.into_iter().chain(cell(&c, c.0.len() + 1))
        });
        f.render_widget(Line::from(spans.collect::<Vec<_>>()), verdict_area);
    } else {
        let half = verdict_area.width / 2;
        for (row, y) in rows.iter().zip(verdict_area.y..) {
            for (c, x) in row.iter().zip([0, half]) {
                let w = if row.len() > 1 {
                    half - 1
                } else {
                    verdict_area.width
                };
                let rect = Rect::new(verdict_area.x + x, y, w, 1).intersection(verdict_area);
                f.render_widget(Line::from(cell(c, 8)), rect);
            }
        }
    }
    // NEXT: the keys that move the task on, the first green, each a hit target for its key
    let mut line = vec![Span::raw("NEXT  ").dim().bold()];
    let mut x = next.x + 6;
    let buttons = next_keys(t);
    if buttons.is_empty() {
        line.push(Span::raw("Nothing needed yet.").dim());
    }
    for (i, (key, label)) in buttons.iter().enumerate() {
        let (chip, text) = match i {
            0 => {
                let s = Style::new().fg(theme.key).bg(theme.green);
                (s.bold(), s)
            }
            _ => {
                let s = Style::new().bg(theme.key);
                (s.fg(theme.accent).bold(), s)
            }
        };
        let button = [
            Span::styled(format!(" {key} "), chip),
            Span::styled(format!(" {label} "), text),
        ];
        let w = Line::from(button.to_vec()).width() as u16;
        let rect = Rect::new(x, next.y, w, 1).intersection(next);
        let target = key_of(key).map(|k| (rect, Target::Key(k), format!("{key} · {label}")));
        app.hits.borrow_mut().targets.extend(target);
        x = x.saturating_add(w + 2);
        line.extend(button);
        line.push(Span::raw("  "));
    }
    f.render_widget(Line::from(line), next);
    // tabs as pills, with badges; each presses its digit, compact's one the next tab's
    let badge = |i: usize| match i {
        1 if t.status == Status::Running => Some(Span::styled(spinner, theme.accent)),
        2 => t.gate.as_ref().map(|_| match gate_passed(t) {
            true => Span::styled(theme.pass, theme.green),
            false => Span::styled(theme.fail, theme.red),
        }),
        FINDINGS => {
            let n = app.findings.actionable().count();
            (n > 0).then(|| Span::styled(n.to_string(), theme.amber))
        }
        _ => None,
    };
    let pill = |i: usize| {
        let mut spans = vec![Span::raw(format!(" {}", TABS[i])).dim()];
        spans.extend(badge(i).map(|b| Span::styled(format!(" {}", b.content), b.style)));
        spans.push(Span::raw(if compact { " ▾ " } else { " " }));
        if i == app.tab {
            let on = Style::new().fg(theme.key).bg(theme.accent).bold();
            spans = spans.into_iter().map(|s| s.style(on)).collect();
        }
        spans
    };
    let shown: Vec<usize> = match compact {
        true => vec![app.tab],
        false => (0..TABS.len()).collect(),
    };
    let mut line = Vec::new();
    let mut x = tabs.x;
    let mut hits = app.hits.borrow_mut();
    hits.tabs = tabs;
    for i in shown {
        let spans = pill(i);
        let w = Line::from(spans.clone()).width() as u16;
        let key = if compact { (i + 1) % TABS.len() } else { i };
        let rect = Rect::new(x, tabs.y, w, 1).intersection(tabs);
        let hint = format!("{} · {}", key + 1, TABS[key]);
        hits.targets.push((rect, Target::Key(digit(key)), hint));
        x = x.saturating_add(w);
        line.extend(spans);
    }
    drop(hits);
    f.render_widget(Line::from(line), tabs);
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
            findings_tab(f, body, app, cursor, theme)
        }
        RUN => run_tab(f, body, t, &app.run, theme, &app.scroll),
        _ => diff_tab(f, body, app, theme),
    }
}

/// `plan ━ queue ━ run ━ check ━ ◉ review ┄ pr`: done stages green, the current one bold in
/// the accent, `✗` in red on a failure and `spinner` while running, later ones dim.
fn pipeline(t: &Task, theme: &Theme, spinner: &'static str) -> Line<'static> {
    let gate_failed = t.gate.iter().flatten().any(|c| !c.passed);
    let at = match t.status {
        Status::Proposed => 0,
        Status::Approved => 1,
        Status::Running => 2,
        Status::Failed if !gate_failed => 2,
        Status::Checking | Status::Failed => 3,
        Status::Review => 4,
        Status::PrOpen | Status::Merged | Status::Discarded => 5,
    };
    let stages = ["plan", "queue", "run", "check", "review", "pr"];
    let mut spans = Vec::new();
    for (i, stage) in stages.into_iter().enumerate() {
        if i > 0 {
            spans.push(match i <= at {
                true => Span::styled(format!(" {} ", theme.done), theme.green),
                false => Span::raw(format!(" {} ", theme.todo)).dim(),
            });
        }
        let (mark, color) = match t.status {
            Status::Failed => (theme.fail, theme.red),
            Status::Running | Status::Checking => (spinner, theme.accent),
            _ => (theme.current, theme.accent),
        };
        spans.push(match i.cmp(&at) {
            std::cmp::Ordering::Less => Span::styled(stage, theme.green),
            std::cmp::Ordering::Greater => Span::raw(stage).dim(),
            std::cmp::Ordering::Equal => {
                Span::styled(format!("{mark} {stage}"), Style::new().fg(color).bold())
            }
        });
    }
    Line::from(spans)
}

/// The verdict as (label, value) cells, spend last: gate dots, critic, diffstat, slot and model;
/// a Running task's context gauge and quiet time in place of the first three.
fn verdict(
    app: &App,
    t: &Task,
    theme: &Theme,
    now: SystemTime,
) -> Vec<(&'static str, Vec<Span<'static>>)> {
    let gauge = |ratio: f64, color: Color| {
        let n = (ratio.clamp(0.0, 1.0) * 8.0).round() as usize;
        vec![
            Span::styled(theme.done.repeat(n), color),
            Span::raw(theme.rest.repeat(8 - n)).dim(),
        ]
    };
    let mut cells = Vec::new();
    if t.status == Status::Running {
        let (window, handoff_at) = app.window;
        if let Some(used) = app.context.filter(|_| window > 0) {
            let ratio = used as f64 / window as f64;
            let color = if ratio > handoff_at {
                theme.amber
            } else {
                theme.accent
            };
            let pct = Span::styled(format!(" {:.0}%", ratio * 100.0), color);
            cells.push(("context", [gauge(ratio, color), vec![pct]].concat()));
        }
        if let Some((_, _, at)) = app.steps.iter().find(|s| s.0 == t.id) {
            let quiet = now.duration_since(*at).unwrap_or_default();
            let late = quiet > app.stall_after / 2;
            let quiet = Span::raw(short(quiet));
            cells.push((
                "quiet",
                vec![if late { quiet.fg(theme.amber) } else { quiet }],
            ));
        }
    } else {
        if let Some(checks) = &t.gate {
            let mut dots: Vec<_> = (checks.iter())
                .map(|c| match c.passed {
                    true => Span::styled(theme.dot, theme.green),
                    false => Span::styled(theme.fail, theme.red),
                })
                .collect();
            let passed = checks.iter().filter(|c| c.passed).count();
            let color = if gate_passed(t) {
                theme.green
            } else {
                theme.red
            };
            dots.push(Span::styled(format!(" {passed}/{}", checks.len()), color));
            cells.push(("gate", dots));
        }
        let fs = &app.findings;
        if *fs != Findings::default() {
            let open = fs.actionable().count();
            let color = if open == 0 { theme.green } else { theme.amber };
            cells.push((
                "critic",
                vec![
                    Span::styled(format!("{open} open"), color),
                    Span::raw(format!(" · {} fixed", fs.fixed.len())).dim(),
                ],
            ));
        }
        if !app.diff.is_empty() {
            let (a, d) = (app.diff.iter()).fold((0, 0), |(a, d), f| (a + f.1, d + f.2));
            cells.push((
                "change",
                vec![
                    Span::styled(format!("+{a}"), theme.green),
                    Span::styled(format!(" -{d}"), theme.red),
                    Span::raw(format!(" · {} files", app.diff.len())).dim(),
                ],
            ));
        }
    }
    let mut at: Vec<_> = t.slot.map(|n| format!("slot {n}")).into_iter().collect();
    at.extend(t.model.as_ref().map(|m| match &t.effort {
        Some(e) => format!("{m}/{e}"),
        None => m.clone(),
    }));
    if !at.is_empty() {
        cells.push(("", vec![Span::raw(at.join(" · ")).dim()]));
    }
    let (spent, budget) = (t.spent(), t.budget_usd.unwrap_or(app.budget));
    let mut spend = match budget > 0.0 {
        true => gauge(spent / budget, theme.accent),
        false => Vec::new(),
    };
    spend.push(Span::raw(match budget > 0.0 {
        true => format!(" ${spent:.2}/${budget:.2}"),
        false => format!("${spent:.2}"),
    }));
    cells.push(("spend", spend));
    cells
}

/// The keys that move `t` on, as (key, label), the first the one to press.
fn next_keys(t: &Task) -> &'static [(&'static str, &'static str)] {
    match t.status {
        Status::Review if gate_passed(t) => &[("m", "open the PR"), ("r", "ask for changes")],
        Status::Review => &[("r", "ask for changes"), ("c", "continue")],
        Status::Failed => &[("t", "retry"), ("c", "continue")],
        Status::Proposed => &[("a", "approve"), ("e", "edit")],
        _ => &[],
    }
}

/// The slot and its ports, each `[scripts]` entry's status, then the run script's output tail.
fn run_tab(f: &mut Frame, area: Rect, t: &Task, run: &RunTab, theme: &Theme, scroll: &Scroll) {
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
    lines.truncate(lines.len() - from_tail(lines.len(), output, scroll));
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
    scroll: &Scroll,
) {
    let label = status_rank(t.status).map_or("", |r| r.1);
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
/// for a question, the answer and the files it cites instead; while planning, the lead's `acts`.
fn request(f: &mut Frame, area: Rect, r: &Request, app: &App, spinner: &str, theme: &Theme) {
    let (answer, acts, scroll) = (app.answer.as_ref(), &app.activity, &app.scroll);
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
    // the lead's live activity under the request, the whole following its tail
    if r.status == Phase::Planning {
        lines.push(Line::raw(""));
        match acts.is_empty() {
            true => lines.push(Line::raw(format!("{spinner} waiting for the lead")).dim()),
            false => lines.extend(act_lines(acts, area.width as usize, theme)),
        }
        let p = Paragraph::new(lines).wrap(Wrap { trim: false });
        let len = p.line_count(area.width);
        let below = from_tail(len, area, scroll);
        let top = len.saturating_sub(area.height as usize + below);
        f.render_widget(p.scroll((top as u16, 0)), area);
        return;
    }
    scrolled(
        f,
        area,
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        scroll,
    );
}

/// Markdown, lightly: headings bold, fenced code in the shell hue, lists, quotes, rules, tables
/// and inline styles; nothing inside a fence is parsed.
fn markdown(text: &str, theme: &Theme) -> Vec<Line<'static>> {
    let mut code = false;
    let mut lines = Vec::new();
    let mut table = Vec::new();
    for l in text.lines() {
        let t = l.trim();
        if !code && t.starts_with('|') {
            table.push(t);
            continue;
        }
        lines.extend(md_table(&std::mem::take(&mut table), theme));
        let (indent, rest) = l.split_at(l.len() - l.trim_start().len());
        let item = ["- ", "* ", "+ "].iter().find_map(|b| rest.strip_prefix(b));
        let quote = rest.strip_prefix("> ").or((t == ">").then_some(""));
        let prefixed = |prefix: String, rest: &str| {
            let mut spans = vec![Span::raw(prefix)];
            spans.extend(inline(rest, Style::new(), theme));
            Line::from(spans)
        };
        if t.starts_with("```") {
            code = !code;
        } else if code {
            lines.push(Line::styled(format!("  {l}"), theme.shell));
        } else if l.starts_with('#') {
            lines.push(Line::raw(l.trim_start_matches('#').trim().to_string()).bold());
        } else if t.len() >= 3 && ['-', '*', '_'].iter().any(|&c| t.chars().all(|x| x == c)) {
            lines.push(Line::raw(theme.rule.repeat(8)).dim());
        } else if let Some(rest) = item {
            lines.push(prefixed(format!("{indent}{} ", theme.bullet), rest));
        } else if let Some(rest) = quote {
            lines.push(prefixed(format!("{indent}{} ", theme.gutter), rest).dim());
        } else {
            lines.push(Line::from(inline(l, Style::new(), theme)));
        }
    }
    lines.extend(md_table(&table, theme));
    lines
}

/// A run of `|` rows with each column padded to its widest cell, the header bold and the
/// delimiter row a rule.
fn md_table(rows: &[&str], theme: &Theme) -> Vec<Line<'static>> {
    let cells = |r: &str| -> Vec<String> {
        let r = r.strip_prefix('|').unwrap_or(r);
        let r = r.strip_suffix('|').unwrap_or(r);
        r.split('|').map(|c| c.trim().to_string()).collect()
    };
    let delimiter = |r: &str| {
        (cells(r).iter()).all(|c| !c.is_empty() && c.chars().all(|x| matches!(x, '-' | ':')))
    };
    let header = rows.get(1).is_some_and(|r| delimiter(r));
    let rows: Vec<Option<Vec<Vec<Span<'static>>>>> = (rows.iter())
        .map(|r| {
            let row = || {
                cells(r)
                    .iter()
                    .map(|c| inline(c, Style::new(), theme))
                    .collect()
            };
            (!delimiter(r)).then(row)
        })
        .collect();
    let width = |cell: &[Span]| cell.iter().map(|s| s.content.width()).sum::<usize>();
    let mut widths: Vec<usize> = Vec::new();
    for (i, cell) in rows.iter().flatten().flat_map(|r| r.iter().enumerate()) {
        if widths.len() <= i {
            widths.push(0);
        }
        widths[i] = widths[i].max(width(cell));
    }
    let total = widths.iter().sum::<usize>() + 2 * widths.len().saturating_sub(1);
    (rows.into_iter().enumerate())
        .map(|(n, row)| {
            let Some(row) = row else {
                return Line::raw(theme.rule.repeat(total)).dim();
            };
            let mut spans = Vec::new();
            let mut pad = 0;
            for (i, cell) in row.into_iter().enumerate() {
                if i > 0 {
                    spans.push(Span::raw(" ".repeat(pad + 2)));
                }
                pad = widths[i] - width(&cell);
                spans.extend(cell);
            }
            match n == 0 && header {
                true => Line::from(spans).bold(),
                false => Line::from(spans),
            }
        })
        .collect()
}

/// `l` with **bold**, *italic*, `code` and [links](url) styled over `base`; `_` inside a word
/// stays literal.
fn inline(l: &str, base: Style, theme: &Theme) -> Vec<Span<'static>> {
    fn flush(spans: &mut Vec<Span<'static>>, text: &mut String, style: Style) {
        if !text.is_empty() {
            spans.push(Span::styled(std::mem::take(text), style));
        }
    }
    let c: Vec<char> = l.chars().collect();
    let find = |from: usize, pat: &[char]| (from..c.len()).find(|&j| c[j..].starts_with(pat));
    let word = |j: usize| c.get(j).is_some_and(|x| x.is_alphanumeric());
    let (mut spans, mut text) = (Vec::new(), String::new());
    let (mut bold, mut italic) = (None, None);
    let mut i = 0;
    while i < c.len() {
        let mut style = base;
        if bold.is_some() {
            style = style.bold();
        }
        if italic.is_some() {
            style = style.italic();
        }
        let ch = c[i];
        if ch == '`'
            && let Some(j) = find(i + 1, &['`'])
        {
            flush(&mut spans, &mut text, style);
            let code: String = c[i + 1..j].iter().collect();
            spans.push(Span::styled(code, style.fg(theme.shell)));
            i = j + 1;
        } else if ch == '['
            && let Some(j) = find(i + 1, &[']'])
            && c.get(j + 1) == Some(&'(')
            && let Some(k) = find(j + 2, &[')'])
        {
            flush(&mut spans, &mut text, style);
            let label: String = c[i + 1..j].iter().collect();
            let url: String = c[j + 2..k].iter().collect();
            spans.push(Span::styled(label, style.underlined()));
            spans.push(Span::styled(format!(" {url}"), base.dim()));
            i = k + 1;
        } else if ch == '*' || ch == '_' {
            let n = if c.get(i + 1) == Some(&ch) { 2 } else { 1 };
            let delim = &[ch, ch][..n];
            let open = if n == 2 { bold } else { italic };
            // A closer follows text, and a single one isn't half of a double.
            let closer = |j: usize| {
                !c[j - 1].is_whitespace()
                    && (ch == '*' || !word(j + n))
                    && (n == 2 || (c[j - 1] != ch && c.get(j + 1) != Some(&ch)))
            };
            let closes = open == Some(ch) && i > 0 && closer(i);
            let opens = open.is_none()
                && c.get(i + n).is_some_and(|x| !x.is_whitespace())
                && (ch == '*' || i == 0 || !word(i - 1))
                && (i + n..c.len()).any(|j| c[j..].starts_with(delim) && closer(j));
            if closes || opens {
                flush(&mut spans, &mut text, style);
                let now = opens.then_some(ch);
                if n == 2 { bold = now } else { italic = now }
            } else {
                text.extend(delim);
            }
            i += n;
        } else {
            text.push(ch);
            i += 1;
        }
    }
    flush(&mut spans, &mut text, base);
    spans
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
/// A location in the diff is a click target that opens it there.
fn findings_tab(f: &mut Frame, area: Rect, app: &App, cursor: Option<usize>, theme: &Theme) {
    let fs = &app.findings;
    let mut lines = Vec::new();
    // (line, column, width, diff file) of each location in the diff
    let mut locs = Vec::new();
    // (line, actionable index) of each actionable finding's row
    let mut rows = Vec::new();
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
            let marked = actionable && app.marked.contains(&i);
            if actionable {
                rows.push((lines.len(), i));
            }
            i += usize::from(actionable);
            let glyph = match x.severity {
                Severity::Blocker => Span::styled(theme.fail, theme.red),
                Severity::Major => Span::styled(theme.checking, theme.amber),
                Severity::Minor => Span::raw(theme.queued).dim(),
            };
            let claim = Span::raw(x.claim.clone());
            let path = path_line(&x.location).0;
            let file = app.diff.iter().position(|d| d.0 == path);
            let at = Span::raw(x.location.clone());
            if let Some(file) = file {
                locs.push((lines.len(), 3 + glyph.width(), at.width(), file));
            }
            lines.push(Line::from(vec![
                Span::styled(if sel { theme.bar } else { " " }, theme.accent),
                Span::styled(if marked { theme.pass } else { " " }, theme.accent),
                glyph,
                Span::raw(" "),
                match file {
                    Some(_) => at.fg(theme.accent).underlined(),
                    None => at.dim(),
                },
                Span::raw(" "),
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
    let wrapped =
        |lines: &[Line<'static>]| Paragraph::new(lines.to_vec()).wrap(Wrap { trim: false });
    scrolled(f, area, wrapped(&lines), &app.scroll);
    let top = app.scroll.get() as usize;
    let y_of = |line: usize| {
        let row = wrapped(&lines[..line]).line_count(area.width);
        row.checked_sub(top).filter(|y| *y < area.height as usize)
    };
    // rows first, so a location on one still wins
    let rows = rows.into_iter().filter_map(|(line, i)| {
        let rect = Rect::new(area.x, area.y + y_of(line)? as u16, area.width, 1);
        Some((rect, Target::Finding(i), "click · select".into()))
    });
    let targets = locs.into_iter().filter_map(|(line, x, w, file)| {
        let rect = Rect::new(area.x + x as u16, area.y + y_of(line)? as u16, w as u16, 1);
        let hint = "click · show it in the diff".into();
        Some((rect.intersection(area), Target::Location(file), hint))
    });
    app.hits.borrow_mut().targets.extend(rows.chain(targets));
}

/// One line per tool call, following the tail: edits in the accent, shell commands in a
/// second hue, reads dim, nudges amber; edit counts and the result on the right, and the
/// agent's text wrapped under a dim gutter.
fn activity_tab(f: &mut Frame, area: Rect, acts: &[Act], theme: &Theme, scroll: &Scroll) {
    if acts.is_empty() {
        f.render_widget(Line::raw("No tool calls yet.").dim(), area);
        return;
    }
    let lines = act_lines(acts, area.width as usize, theme);
    let end = lines.len() - from_tail(lines.len(), area, scroll);
    let start = end.saturating_sub(area.height as usize);
    f.render_widget(Paragraph::new(lines[start..end].to_vec()), area);
}

/// The Activity tab's lines for `acts`, each at most `width` columns.
fn act_lines(acts: &[Act], width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for a in acts {
        if a.tool == TEXT {
            let gutter = format!("  {} ", theme.gutter);
            for l in a
                .target
                .lines()
                .flat_map(|l| wrap(l, width.saturating_sub(4)))
            {
                let l = truncate(&l, width.saturating_sub(4), theme.ellipsis);
                lines.push(Line::from(vec![Span::raw(gutter.clone()), Span::raw(l)]).dim());
            }
            continue;
        }
        let mut right = Vec::new();
        if let Some((add, del)) = a.edit {
            right.push(Span::styled(format!(" +{add}"), theme.green));
            right.push(Span::styled(format!(" -{del}"), theme.red));
        }
        match (a.ok, a.exit) {
            (Some(true), _) => right.push(Span::styled(format!("  {}", theme.pass), theme.green)),
            (Some(false), Some(n)) => right.push(Span::styled(
                format!("  {} exit {n}", theme.fail),
                theme.red,
            )),
            (Some(false), None) => right.push(Span::styled(format!("  {}", theme.fail), theme.red)),
            (None, _) => {}
        }
        let right_w: usize = right.iter().map(|s| s.width()).sum();
        let (verb, style) = verb(&a.tool, theme);
        let room = width.saturating_sub(verb.width().max(7) + 1 + right_w);
        let target = truncate(&a.target, room, theme.ellipsis);
        let pad = " ".repeat(room.saturating_sub(target.width()));
        let target = match a.tool.as_str() {
            NUDGE => Span::styled(target, style),
            _ => Span::raw(target).dim(),
        };
        let left = [Span::styled(format!("{verb:<7} "), style), target];
        lines.push(Line::from([&left[..], &[Span::raw(pad)], &right].concat()));
    }
    lines
}

/// `s` split at spaces into lines of at most `width` columns, where its words fit.
fn wrap(s: &str, width: usize) -> Vec<String> {
    let mut out = vec![String::new()];
    for word in s.split_whitespace() {
        let last = out.last_mut().expect("starts non-empty");
        if last.is_empty() {
            last.push_str(word);
        } else if last.width() + 1 + word.width() > width {
            out.push(word.into());
        } else {
            last.push(' ');
            last.push_str(word);
        }
    }
    out
}

/// The Activity verb and style for a tool call's tool.
fn verb<'a>(tool: &'a str, theme: &Theme) -> (&'a str, Style) {
    match tool {
        "Edit" | "NotebookEdit" => ("edit", Style::new().fg(theme.accent)),
        "Write" => ("write", Style::new().fg(theme.accent)),
        "Bash" => ("run", Style::new().fg(theme.shell)),
        "Read" => ("read", Style::new().dim()),
        "Grep" | "Glob" => ("search", Style::new().dim()),
        "WebSearch" => ("web", Style::new().dim()),
        "WebFetch" => ("fetch", Style::new().dim()),
        NUDGE => ("↻ nudged", Style::new().fg(theme.amber)),
        HANDOFF => ("⇢ handoff", Style::new()),
        other => (other, Style::new()),
    }
}

/// The checks as a table, then the tail of each failing step's output from the gate log.
fn gate_tab(f: &mut Frame, area: Rect, t: &Task, log: &str, theme: &Theme, scroll: &Scroll) {
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
    lines.truncate(lines.len() - from_tail(lines.len(), output, scroll));
    let skip = lines.len().saturating_sub(output.height as usize);
    f.render_widget(Paragraph::new(lines.split_off(skip)), output);
}

/// git-style `+`/`-` counts with bars scaled to the largest change, and the unfolded files'
/// hunks with each finding under its line; `cursor` marks the file `enter` folds, and stays in
/// view.
fn diff_tab(f: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let files = &app.diff;
    if files.is_empty() {
        f.render_widget(Line::raw("No changes yet.").dim(), area);
        return;
    }
    let cursor = app.on_diff().then_some(app.diff_file);
    let (added, deleted) = files.iter().fold((0, 0), |(a, d), f| (a + f.1, d + f.2));
    let max = files.iter().map(|f| f.1 + f.2).max().unwrap_or(1).max(1);
    let bar_width = 20u64.min(max);
    let bar = if cursor.is_some() { 2 } else { 0 };
    let path_width = (area.width as usize).saturating_sub(15 + bar + bar_width as usize);
    let scale = |n: u64| ((n * bar_width).div_ceil(max)) as usize;
    let fs = &app.findings;
    let lists = [
        ("open", &fs.findings),
        ("disputed", &fs.disputed),
        ("optional", &fs.optional),
        ("fixed", &fs.fixed),
        ("waived", &fs.waived),
    ];
    let found: Vec<_> = (lists.into_iter())
        .flat_map(|(state, list)| list.iter().map(move |x| (state, path_line(&x.location), x)))
        .collect();
    let mut lines: Vec<Line> = Vec::new();
    // each file's line, and the cursor file's first and last line
    let (mut rows, mut section) = (Vec::new(), (0, 0));
    for (i, (path, a, d)) in files.iter().enumerate() {
        let open = app.unfolded.contains(&i);
        rows.push(lines.len());
        let sel = cursor == Some(i);
        let mark = match cursor {
            Some(_) => format!("{} ", if sel { theme.bar } else { " " }),
            None => String::new(),
        };
        let fold = if open { theme.fold.0 } else { theme.fold.1 };
        let name = truncate(path, path_width, theme.ellipsis);
        let name = Span::raw(format!("{name:<path_width$} "));
        lines.push(Line::from(vec![
            Span::styled(mark, theme.accent),
            Span::raw(format!("{fold} ")).dim(),
            if sel { name.bold() } else { name },
            Span::styled(format!("{:>5}", format!("+{a}")), theme.green),
            Span::styled(format!("{:>6} ", format!("-{d}")), theme.red),
            Span::styled("+".repeat(scale(*a)), theme.green),
            Span::styled("-".repeat(scale(*d)), theme.red),
        ]));
        let mut new = 0;
        for l in app.hunks.get(i).filter(|_| open).into_iter().flatten() {
            if l.starts_with("@@") {
                // `@@ -a,b +c,d @@`: the new file's lines count from c
                let c = l
                    .split(" +")
                    .nth(1)
                    .and_then(|r| r.split([',', ' ']).next());
                new = c.and_then(|c| c.parse().ok()).unwrap_or(0);
                lines.push(Line::styled(format!("    {l}"), theme.shell));
                continue;
            }
            let (n, style) = match l.chars().next() {
                Some('+') => (Some(new), Style::new().fg(theme.green)),
                Some('-') => (None, Style::new().fg(theme.red)),
                Some(' ') => (Some(new), Style::new()),
                _ => (None, Style::new().dim()),
            };
            let num = n.map_or(String::new(), |n| n.to_string());
            lines.push(Line::from(vec![
                Span::raw(format!("  {num:>4} ")).dim(),
                Span::styled(l.clone(), style),
            ]));
            let Some(n) = n else { continue };
            new += 1;
            let here = found
                .iter()
                .filter(|(_, at, _)| at.0 == *path && at.1 == Some(n));
            for (state, _, x) in here {
                let head = format!("       {} critic, {state}  ", theme.checking);
                lines.push(Line::from(vec![
                    Span::styled(head, theme.amber),
                    Span::raw(format!("{}  {}", x.location, x.claim)).dim(),
                ]));
            }
        }
        if sel {
            section = (rows[i], lines.len() - 1);
        }
    }
    lines.push(Line::raw(""));
    let total = format!("{} files changed, +{added} -{deleted}", files.len());
    lines.push(Line::raw(total).dim());
    if cursor.is_some() && app.follow.take() {
        let (first, last) = section;
        let lo = (first + 1).saturating_sub(area.height as usize);
        let top = (app.scroll.get() as usize).max(lo).min(last);
        app.scroll.set(top as u16);
    }
    scrolled(f, area, Paragraph::new(lines), &app.scroll);
    let top = app.scroll.get() as usize;
    let targets = rows.into_iter().enumerate().filter_map(|(i, row)| {
        let y = row.checked_sub(top).filter(|y| *y < area.height as usize)?;
        let rect = Rect::new(area.x, area.y + y as u16, area.width, 1);
        Some((
            rect,
            Target::File(i),
            "click · show or hide its hunks".into(),
        ))
    });
    app.hits.borrow_mut().targets.extend(targets);
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

    fn act(tool: &str, target: &str) -> Act {
        Act {
            tool: tool.into(),
            target: target.into(),
            ..Default::default()
        }
    }

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
            palette: None,
            compose: None,
            notice: None,
            info: None,
            info_since: None,
            opening: false,
            tab: 0,
            activity: Vec::new(),
            gate_log: String::new(),
            diff: Vec::new(),
            hunks: Vec::new(),
            unfolded: BTreeSet::new(),
            loaded: None,
            confirm: None,
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
            scroll: Scroll::default(),
            findings: Findings::default(),
            finding: 0,
            marked: BTreeSet::new(),
            disputed: Vec::new(),
            sessions: 3,
            run: RunTab::default(),
            serving: Vec::new(),
            steps: Vec::new(),
            stall_after: Duration::from_secs(15 * 60),
            settings: None,
            vscode: false,
            editing: None,
            open_at: None,
            diff_file: 0,
            follow: Cell::new(false),
            zoom: false,
            last_click: None,
            split: 45,
            dragging: false,
            context: None,
            budget: 5.0,
            window: (200_000, 0.8),
            folded: [false; 3],
            slots: 3,
            hits: RefCell::default(),
            hover: None,
            menu: None,
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
        let mut term = Terminal::new(TestBackend::new(60, 17)).unwrap();
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
                "╭ Task ──────────────────────────────────────────── z zoom ╮",
                "│ plan ━ queue ━ run ━ check ━ ◉ review ┄ pr               │",
                "│ gate ● 1/1  ·  slot 1 · opus/high  ·  spend ──────── $0. │",
                "│ NEXT   m  open the PR    r  ask for changes              │",
                "│  Summary ▾                                               │",
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
                " yogan   ● 2    m  open PR  M  merge to main  ?  more       ",
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
        app.activity = (1..=30).map(|i| act("Read", &format!("f{i}.rs"))).collect();
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
        let call = act;
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
                "╭ Task ──────────────────────────────────────────── z zoom ╮",
                "│ plan ━ queue ━ run ━ check ━ ◉ review ┄ pr               │",
                "│ gate ● 1/1  ·  slot 1 · opus/high  ·  spend ──────── $0. │",
                "│ NEXT   m  open the PR    r  ask for changes              │",
                "│  Activity ▾                                              │",
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
                " yogan   ● 2    m  open PR  M  merge to main  ?  more       ",
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
                "╭ Task ──────────────────────────────────────────── z zoom ╮",
                "│ plan ━ queue ━ run ━ check ━ ◉ review ┄ pr               │",
                "│ gate ●●✗ 2/3  ·  slot 1 · opus/high  ·  spend ──────── $ │",
                "│ NEXT   r  ask for changes    c  continue                 │",
                "│  Gate ✗ ▾                                                │",
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
                " yogan   ● 2    r  reply to worker  d  diff  ?  more        ",
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
                "╭ Task ──────────────────────────────────────────── z zoom ╮",
                "│ plan ━ queue ━ run ━ check ━ ◉ review ┄ pr               │",
                "│ gate ● 1/1  ·  slot 1 · opus/high  ·  spend ──────── $0. │",
                "│ NEXT   m  open the PR    r  ask for changes              │",
                "│  Run ▾                                                   │",
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
                " yogan   ● 2    m  open PR  M  merge to main  ?  more       ",
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
        s.row = 16;
        s.cycle(true); // stall after: 15m to 20m
        s.row = 20;
        s.cycle(false); // handoff at: 0.8 to 0.75
        s.row = 10;
        s.cycle(false); // concurrency: 4 to 3
        s.row = 25;
        s.cycle(true); // mouse: true to false
        s.row = 4;
        s.cycle(true); // critic: fable wraps round to opus

        let (mut app, _) = app();
        app.settings = Some(s);
        let mut term = Terminal::new(TestBackend::new(60, 41)).unwrap();
        let theme = Theme::new(false, false);
        term.draw(|f| draw(f, &app, &theme, 0, SystemTime::UNIX_EPOCH))
            .unwrap();
        assert_eq!(
            screen(&term),
            [
                " yogan · fuse-os   ● 2 need you   ⠋ 1 working  ○ 1 queued   ",
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
                "│ Workers                                                  │",
                "│   concurrency        3 •                                 │",
                "│   slots              5                                   │",
                "│   budget usd         5.0                                 │",
                "│   cargo builds       2                                   │",
                "│   fix rounds         2                                   │",
                "│   questions at once  2                                   │",
                "│                                                          │",
                "│ Watch                                                    │",
                "│   stall after        20m •                               │",
                "│   nudges             1                                   │",
                "│   loop repeats       4                                   │",
                "│   autocompact        200000                              │",
                "│   handoff at         0.75 •                              │",
                "│   max handoffs       2                                   │",
                "│                                                          │",
                "│ Disk                                                     │",
                "│   max target gb      60                                  │",
                "│   min free gb        100                                 │",
                "│                                                          │",
                "│ Other                                                    │",
                "│   draft PRs          true                                │",
                "│   mouse              false •                             │",
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
                .any(|r| r.contains("mouse") && r.contains("‹ false ›")),
            "{rows:#?}"
        );

        let mut s = app.settings.take().unwrap();
        let path = s.save(&checkout, &home).unwrap();
        let again = Settings::load(&checkout, &home, false).unwrap();
        assert_eq!(again.values, s.values);
        assert_eq!(again.loaded, again.values);
        // only the changed keys are written
        let written: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(written["worker"].as_table().unwrap().len(), 2);
        assert_eq!(written["tui"]["mouse"].as_bool(), Some(false));
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
                "╭ Task ──────────────────────────────────────────── z zoom ╮",
                "│ plan ━ queue ━ run ━ check ━ ◉ review ┄ pr               │",
                "│ gate ● 1/1  ·  change +52 -3 · 3 files  ·  slot 1 · opus │",
                "│ NEXT   m  open the PR    r  ask for changes              │",
                "│  Diff ▾                                                  │",
                "│ ▌ ▸ src/config.rs         +12    -3 ++++++--             │",
                "│   ▸ crates/ledger/src/…   +40    -0 ++++++++++++++++++++ │",
                "│   ▸ assets/logo.png        +0    -0                      │",
                "│                                                          │",
                "│ 3 files changed, +52 -3                                  │",
                "│                                                          │",
                "│                                                          │",
                "│                                                          │",
                "│                                                          │",
                "│                                                          │",
                "╰──────────────────────────────────────────────────────────╯",
                " yogan   ● 2    j/k  file  enter  fold  ?  more             ",
            ]
        );
    }

    #[test]
    fn diff_tab_unfolded_file_with_a_finding() {
        let (mut app, _) = app();
        app.diff = vec![("src/config.rs".into(), 2, 1), ("README.md".into(), 1, 0)];
        app.hunks = vec![
            [
                "@@ -40,3 +40,4 @@ impl Retry",
                " fn parse(raw: &Raw) {",
                "-    let d = raw.max as u64;",
                "+    ensure!(raw.max >= 0);",
                "+    let d = u64::try_from(raw.max)?;",
                " }",
            ]
            .map(String::from)
            .into(),
            vec!["@@ -1 +1,2 @@".into()],
        ];
        app.unfolded.insert(0);
        app.findings.fixed = vec![crate::critic::Finding {
            severity: Severity::Minor,
            location: "src/config.rs:42".into(),
            claim: "as u64 wraps".into(),
            evidence: String::new(),
            reply: None,
        }];
        assert_eq!(
            tab_screen(app, 4)[5..15],
            [
                "│ ▌ ▾ src/config.rs                           +2    -1 ++- ┃",
                "│     @@ -40,3 +40,4 @@ impl Retry                         ┃",
                "│     40  fn parse(raw: &Raw) {                            ┃",
                "│        -    let d = raw.max as u64;                      ┃",
                "│     41 +    ensure!(raw.max >= 0);                       ┃",
                "│     42 +    let d = u64::try_from(raw.max)?;             ┃",
                "│        ◆ critic, fixed  src/config.rs:42  as u64 wraps   ┃",
                "│     43  }                                                ┃",
                "│   ▸ README.md                               +1    -0 +   ┃",
                "│                                                          │",
            ]
        );
    }

    #[test]
    fn clicking_a_finding_location_opens_it_in_diff() {
        let (mut app, _) = app();
        app.findings = sample_findings();
        app.diff = vec![("src/lib.rs".into(), 1, 0), ("src/retry.rs".into(), 3, 1)];
        (app.detail, app.tab) = (false, FINDINGS);
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let theme = Theme::new(false, false);
        term.draw(|f| draw(f, &app, &theme, 0, SystemTime::UNIX_EPOCH))
            .unwrap();
        let rows = screen(&term);
        // the rest of a row selects its finding
        let row = rows
            .iter()
            .position(|r| r.contains("may overflow"))
            .unwrap();
        let column = rows[row][..rows[row].find("may overflow").unwrap()]
            .chars()
            .count();
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: column as u16 + 2,
            row: row as u16,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!((app.tab, app.finding), (FINDINGS, 2));
        let row = rows
            .iter()
            .position(|r| r.contains("src/retry.rs:9"))
            .unwrap();
        let column = rows[row][..rows[row].find("src/retry.rs:9").unwrap()]
            .chars()
            .count();
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: column as u16 + 2,
            row: row as u16,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.tab, DIFF);
        assert!(app.on_diff());
        assert_eq!(app.diff_file, 1);
        assert_eq!(app.unfolded, BTreeSet::from([1]));
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
        app.awaiting = Some(("t1".into(), None, 1, false));
        app.check_awaiting(false);
        assert!(app.awaiting.is_some() && !app.preview, "still drafting");
        let err = app.draft(None).unwrap_err().to_string();
        assert_eq!(err, "a PR draft is on its way");

        // a new draft opens the preview on its task, once no modal would act on it
        app.selected = 2;
        app.tasks[0].0.pr_draft = Some(draft.clone());
        app.confirm = Some('x');
        app.check_awaiting(false);
        assert!(app.awaiting.is_some() && !app.preview && app.selected == 2);
        app.confirm = None;
        app.check_awaiting(false);
        assert!(app.awaiting.is_none() && app.preview && app.selected == 0);

        // the worker exited without a new draft: its error is shown
        let state = std::env::temp_dir().join(format!("yogan-awaiting-{}", std::process::id()));
        fs::create_dir_all(state.join("logs")).unwrap();
        fs::write(state.join("logs/t1.pr.log"), "claude exited with 1\n").unwrap();
        app.state = state.clone();
        app.awaiting = Some(("t1".into(), Some(draft.clone()), 1, false));
        app.check_awaiting(true);
        assert!(app.awaiting.is_none());
        assert_eq!(
            app.notice.take().as_deref(),
            Some("no PR draft: claude exited with 1")
        );
        fs::remove_dir_all(&state).unwrap();

        // a failed task or gate gives up; so does a task that's gone
        app.awaiting = Some(("t2".into(), None, 1, false));
        app.check_awaiting(false);
        assert!(app.awaiting.is_none() && app.notice.take().is_some());
        app.tasks[0].0.gate.as_mut().unwrap()[0].passed = false;
        app.awaiting = Some(("t1".into(), Some(draft), 1, false));
        app.check_awaiting(false);
        assert!(app.awaiting.is_none() && app.notice.take().is_some());
        app.awaiting = Some(("gone".into(), None, 1, false));
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
                "╭ Tasks ────────────────────────────────────╮╭ Task ─────────────────────────────────────── z zoom ╮",
                "│   ✓ Reject negative max_delay      ready  ││ ◉ plan ┄ queue ┄ run ┄ check ┄ review ┄ pr          │",
                "│   ○ Validate max_delay at…  2 to approve  ││ spend ──────── $0.00/$5.00                          │",
                "│ ▌ ○ └ Reject negative max_delay in the …  ││ NEXT   a  approve    e  edit                        │",
                "│   ○ Bump sqlx to 0.9        1 to approve  ││  Summary ▾                                          │",
                "│                                           ││ Reject negative max_delay in the CLI                │",
                "│                                           ││ Proposed · CC-687 · cli, config · after “Validate   │",
                "│                                           ││ max_delay at parse time”                            │",
                "│                                           ││                                                     │",
                "│                                           ││ Reuse the config check in the CLI.                  │",
                "│                                           ││                                                     │",
                "│                                           ││ Done when                                           │",
                "│                                           ││ - `yogan --max-delay -1` exits 2                    │",
                "│                                           ││ - the error names the flag                          │",
                "╰───────────────────────────────────────────╯╰─────────────────────────────────────────────────────╯",
                " yogan   ● 4    a  approve  A  approve all  e  edit  r  reply to lead  x  discard task  ?  more     ",
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
                "╭ Tasks ────────────────────────────────────╮╭ Task ─────────────────────────────────────── z zoom ╮",
                "│   ✓ Reject negative max_delay   ready  4m ││ Reject a negative max_delay                         │",
                "│   ✗ Bump sqlx to 0.9           failed  1h ││ Failed · CC-687                                     │",
                "│ ▌ ✗ Reject a negative max_delay   failed  ││                                                     │",
                "│   ⠋ Retry webhook sends     working · 12m ││ claude exited with exit status: 1                   │",
                "│   ⠋ Split the ledger job into p… planning ││                                                     │",
                "│   ○ Split the ledger reconc… needs a slot ││ Reject a negative max_delay                         │",
                "│                                           ││ It panics in the retry loop.                        │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "╰───────────────────────────────────────────╯╰─────────────────────────────────────────────────────╯",
                " yogan   ● 3    t  retry  x  discard  n  new task  j/k  move  tab  pane  q  quit  ?  more           ",
            ]
        );
        assert!(app.task().is_none());
        app.selected = 2;
        assert_eq!(app.task().unwrap().0.id, "t1");
    }

    #[test]
    fn running_rows_show_the_step() {
        let (mut app, now) = app();
        let mut stuck = app.tasks[2].clone();
        stuck.0.id = "t5".into();
        stuck.0.title = "Cache the rate table".into();
        app.tasks.insert(3, stuck);
        let ago = |m: u64| now - Duration::from_secs(m * 60);
        let call = |tool: &str, target: &str| (tool.to_string(), target.to_string());
        app.steps = vec![
            ("t3".into(), call("Edit", "src/retry.rs"), ago(2)),
            ("t5".into(), call("Bash", "cargo test -p rates"), ago(9)),
        ];
        let mut term = Terminal::new(TestBackend::new(100, 12)).unwrap();
        let theme = Theme::new(false, false);
        term.draw(|f| draw(f, &app, &theme, 0, now)).unwrap();
        assert_eq!(
            screen(&term),
            [
                "╭ Tasks ────────────────────────────────────╮╭ Task ─────────────────────────────────────── z zoom ╮",
                "│ ▌ ✓ Reject negative max_delay   ready  4m ││ plan ━ queue ━ run ━ check ━ ◉ review ┄ pr          │",
                "│   ✗ Bump sqlx to 0.9           failed  1h ││ gate ● 1/1  ·  slot 1 · opus/high  ·  spend ─────── │",
                "│   ⠋ Retry webhook sends     working · 12m ││ NEXT   m  open the PR    r  ask for changes         │",
                "│       edit src/retry.rs          quiet 2m ││  Summary ▾                                          │",
                "│   ⠋ Cache the rate table    working · 12m ││ Reject negative max_delay                           │",
                "│       run cargo test -p rates    quiet 9m ││ Review · u/reject-negative · slot 1 · opus/high     │",
                "│   ○ Split the ledger reconc… needs a slot ││                                                     │",
                "│                                           ││ max_delay below zero now fails at parse time.       │",
                "│                                           ││                                                     │",
                "╰───────────────────────────────────────────╯╰─────────────────────────────────────────────────────╯",
                " yogan   ● 2    m  open PR  M  merge to main  r  reply to worker  d  diff  x  discard task  ?  more ",
            ]
        );
        // past half of the 15m stall_after, the quiet time turns amber
        let buf = term.backend().buffer();
        assert_ne!(buf[(36, 4)].fg, theme.amber);
        assert_eq!(buf[(36, 6)].fg, theme.amber);
    }

    #[test]
    fn merging_to_main() {
        let (mut app, now) = app();
        let press = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        assert!(
            app.commands()
                .iter()
                .any(|c| c.0 == "M" && c.1 == "merge to main")
        );
        assert!(app.key(press('M')) && app.confirm == Some('M'));
        let mut term = Terminal::new(TestBackend::new(60, 12)).unwrap();
        let theme = Theme::new(false, false);
        term.draw(|f| draw(f, &app, &theme, 0, now)).unwrap();
        assert_eq!(
            screen(&term),
            [
                "╭ Tasks ───────────────────────────────────────────────────╮",
                "│ ▌ ✓ Reject negative max_delay                  ready  4m │",
                "│   ✗ Bump sqlx to 0.9                          failed  1h │",
                "│   ╭ Merge ───────────────────────────────────────────╮2m │",
                "│   │ Merge “Reject negative max_delay”?               │ot │",
                "│   │ rebase, re-gate and push it straight to main,    │   │",
                "│   │ with no PR                                       │   │",
                "│   │  y  Merge     esc  Cancel                        │   │",
                "│   ╰──────────────────────────────────────────────────╯   │",
                "│                                                          │",
                "╰──────────────────────────────────────────────────────────╯",
                " yogan   ● 2    m  open PR  M  merge to main  ?  more       ",
            ]
        );
        // esc starts nothing
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.confirm.is_none() && app.awaiting.is_none());

        // not until the parent is Merged, nor for a task not in Review
        app.tasks[0].0.parent = Some("t2".into());
        assert!(!actions(&app).iter().any(|(k, _)| *k == "M"));
        assert!(app.key(press('M')) && app.confirm.is_none());
        app.tasks[1].0.status = Status::Merged;
        assert!(valid(&app, "M"));
        app.selected = 2;
        assert!(!valid(&app, "M"));

        // a merge worker that exits with the task in Review shows why
        let state = std::env::temp_dir().join(format!("yogan-merging-{}", std::process::id()));
        fs::create_dir_all(state.join("logs")).unwrap();
        fs::write(state.join("logs/t1.pr.log"), "the gate failed\n").unwrap();
        app.state = state.clone();
        app.awaiting = Some(("t1".into(), None, 1, true));
        app.check_awaiting(false);
        assert!(app.awaiting.is_some() && app.notice.is_none());
        // re-gating a moved head passes through Checking
        app.tasks[0].0.status = Status::Checking;
        app.check_awaiting(false);
        assert!(app.awaiting.is_some() && app.notice.is_none());
        app.tasks[0].0.status = Status::Review;
        app.check_awaiting(true);
        assert!(app.awaiting.is_none());
        let notice = app.notice.take();
        assert_eq!(notice.as_deref(), Some("not merged: the gate failed"));
        fs::remove_dir_all(&state).unwrap();
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
        assert!(app.key(press('x')) && app.confirm.is_some());
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
                "╭ Tasks ────────────────────────────────────╮╭ Task ─────────────────────────────────────── z zoom ╮",
                "│   ✓ Reject negative max_delay   ready  4m ││ How are tasks saved?                                │",
                "│   ✗ Bump sqlx to 0.9           failed  1h ││ Question                                            │",
                "│ ▌ ✓ How are tasks saved?        answered  ││                                                     │",
                "│   ⠋ Retry webhook sends     working · 12m ││ Atomically                                          │",
                "│   ○ Split the ledger reconc… needs a slot ││ A tmp file, then a rename:                          │",
                "│                                           ││   fs::rename(&tmp, &path)                           │",
                "│                                           ││                                                     │",
                "│                                           ││ Cites                                               │",
                "│                                           ││ - src/task.rs:72                                    │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "╰───────────────────────────────────────────╯╰─────────────────────────────────────────────────────╯",
                " yogan   ● 3    p  plan it  r  reply to lead  y  copy  x  discard  n  new task  j/k  move  ?  more  ",
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
    fn answering_screen() {
        let (mut app, now) = app();
        let state = std::env::temp_dir().join(format!("yogan-answering-{}", std::process::id()));
        let asking = Request {
            id: "r6".into(),
            // a long word wraps to more rows than it has lines
            text: format!("How are tasks saved?\nhttps://{}.rs", "x".repeat(100)),
            mode: Mode::Ask,
            status: Phase::Planning,
            ..Default::default()
        };
        asking.save(&state).unwrap();
        (app.state, app.repo) = (state.clone(), "/repo".into());
        let draw_rows = |app: &App| {
            let mut term = Terminal::new(TestBackend::new(100, 12)).unwrap();
            term.draw(|f| draw(f, app, &Theme::new(false, false), 0, now))
                .unwrap();
            screen(&term)
        };
        app.reload(&state).unwrap();
        app.selected = 0;
        app.load_tab();
        let rows = draw_rows(&app);
        assert!(rows[9].contains("⠋ waiting for the lead"), "{rows:#?}");

        let call = |id: &str, name: &str, input: &str| {
            format!(
                r#"{{"type":"assistant","message":{{"id":"m{id}","usage":{{}},"content":[{{"type":"tool_use","id":"{id}","name":"{name}","input":{input}}}]}}}}"#
            )
        };
        let lines = [
            call("a", "Grep", r#"{"pattern":"fn save"}"#),
            call("b", "Read", r#"{"file_path":"/repo/src/task.rs"}"#),
            r#"{"type":"assistant","message":{"id":"t","usage":{},"content":[{"type":"text","text":"Saves go through a tmp file."}]}}"#.into(),
            call("c", "WebSearch", r#"{"query":"atomic rename posix"}"#),
            call("d", "WebFetch", r#"{"url":"https://man7.org/rename.2.html"}"#),
        ];
        fs::create_dir_all(state.join("logs")).unwrap();
        fs::write(state.join("logs/r6.jsonl"), lines.join("\n")).unwrap();
        app.reload(&state).unwrap();
        app.load_tab();
        app.steps[0].2 = now - Duration::from_secs(2 * 60);
        fs::remove_dir_all(&state).unwrap();
        assert_eq!(
            draw_rows(&app),
            [
                "╭ Tasks ────────────────────────────────────╮╭ Task ─────────────────────────────────────── z zoom ╮",
                "│ ▌ ⠋ How are tasks saved?        answering ││ https://xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx │",
                "│       fetch https://man7.org/re… quiet 2m ││ xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx │",
                "│                                           ││ xxxxxx.rs                                           │",
                "│                                           ││                                                     ┃",
                "│                                           ││ search  fn save                                     ┃",
                "│                                           ││ read    src/task.rs                                 ┃",
                "│                                           ││   │ Saves go through a tmp file.                    ┃",
                "│                                           ││ web     atomic rename posix                         ┃",
                "│                                           ││ fetch   https://man7.org/rename.2.html              ┃",
                "╰───────────────────────────────────────────╯╰─────────────────────────────────────────────────────╯",
                " yogan  ✓ nothing needs you   n  new task  j/k  move  tab  pane  q  quit  ?  more                   ",
            ]
        );
        // scrolling back reaches the whole request
        app.key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        let rows = draw_rows(&app);
        assert!(rows[1].contains("│ How are tasks saved?"), "{rows:#?}");
        assert!(rows[7].contains("xxx.rs"), "{rows:#?}");
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
        let mut term = Terminal::new(TestBackend::new(60, 32)).unwrap();
        let theme = Theme::new(false, false);
        term.draw(|f| draw(f, &app, &theme, 0, SystemTime::UNIX_EPOCH))
            .unwrap();
        assert_eq!(
            screen(&term),
            [
                " yogan · fuse-os   ● 2 need you   ⠋ 1 working  ○ 1 queued   ",
                "╭ Task ──────────────────────────────────────────── z zoom ╮",
                "│ plan ━ queue ━ run ━ check ━ ◉ review ┄ pr               │",
                "│                                                          │",
                "│ gate    ● 1/1               critic  3 open · 0 fixed     │",
                "│ slot 1 · opus/high                                       │",
                "│ spend   ──────── $0.00/$5.00                             │",
                "│                                                          │",
                "│ NEXT   m  open the PR    r  ask for changes              │",
                "│                                                          │",
                "│  Summary  Activity  Gate ✓  Findings 3  Diff  Run        │",
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
                "│                                                          │",
                "│                                                          │",
                "╰──────────────────────────────────────────────────────────╯",
                " j/k  finding  space  mark  f  fix  F  fix all  ?  more     ",
            ]
        );
    }

    #[test]
    fn findings_keys_ignore_focus() {
        let (mut app, _) = app();
        app.findings = sample_findings();
        (app.finding, app.detail, app.tab) = (1, false, FINDINGS);
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let theme = Theme::new(false, false);
        term.draw(|f| draw(f, &app, &theme, 0, SystemTime::UNIX_EPOCH))
            .unwrap();
        assert_eq!(
            screen(&term)[29],
            " j/k  finding  space  mark  f  fix  F  fix all  r  reply to worker  x  waive  o  open  ?  more      "
        );
        app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(app.reply_for, Some('x'));
        assert!(app.confirm.is_none());
    }

    #[test]
    fn menu_and_preview_clicks_do_what_they_say() {
        let (mut app, now) = app();
        let theme = Theme::new(false, false);
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut send = |app: &mut App, kind, text: &str| {
            term.draw(|f| draw(f, app, &theme, 0, now)).unwrap();
            let rows = screen(&term);
            let hit = rows.iter().enumerate().find(|(_, r)| r.contains(text));
            let (y, row) = hit.unwrap_or_else(|| panic!("no {text:?} in {rows:#?}"));
            let x = row[..row.find(text).unwrap()].chars().count();
            let m = MouseEvent {
                kind,
                column: x as u16,
                row: y as u16,
                modifiers: KeyModifiers::NONE,
            };
            app.mouse(m);
            rows
        };
        let left = MouseEventKind::Down(MouseButton::Left);

        // on Findings, the menu's discard opens the confirm, and a double-click doesn't answer it
        (app.findings, app.tab) = (sample_findings(), FINDINGS);
        send(
            &mut app,
            MouseEventKind::Down(MouseButton::Right),
            "Reject negative",
        );
        send(&mut app, left, "x   discard task");
        assert!(app.confirm == Some('x') && app.reply_for.is_none());
        let m = app.last_click.unwrap().0;
        app.mouse(MouseEvent {
            kind: left,
            column: m.x,
            row: m.y,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.confirm.is_some());

        // the preview's footer is the preview's, even on the Diff tab
        app.confirm = None;
        app.tasks[0].0.pr_draft = Some(pr::Draft {
            title: "feat(config): reject negative max_delay".into(),
            body: "max_delay below zero now fails at parse time.".into(),
            head: "abc".into(),
            problem: None,
        });
        (app.preview, app.detail, app.tab) = (true, true, DIFF);
        let rows = send(&mut app, left, "PR preview");
        assert!(rows[29].contains("enter  push and open PR"), "{}", rows[29]);
    }

    #[test]
    fn diff_hunks_line_up_with_numstat() {
        let repo = std::env::temp_dir().join(format!("yogan-hunks-{}", std::process::id()));
        let _ = fs::remove_dir_all(&repo);
        fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        fs::write(repo.join("a.txt"), "a\n").unwrap();
        fs::write(repo.join("r.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        fs::write(repo.join("z.txt"), "z\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        // a rename with an edit, a file turned symlink, and an edit after both
        git(&["mv", "r.txt", "s.txt"]);
        fs::write(repo.join("s.txt"), "one\ntwo\nthree\nfour\nfive\n").unwrap();
        fs::remove_file(repo.join("a.txt")).unwrap();
        std::os::unix::fs::symlink("z.txt", repo.join("a.txt")).unwrap();
        fs::write(repo.join("z.txt"), "zz\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "change"]);

        let stat = diffstat(&repo, "HEAD~1");
        let hunks = hunks(&repo, "HEAD~1");
        assert_eq!(stat.len(), hunks.len(), "{stat:?}");
        assert!(stat.iter().all(|(p, ..)| !p.contains("=>")), "{stat:?}");
        let z = stat.iter().position(|(p, ..)| p == "z.txt").unwrap();
        assert!(hunks[z].iter().any(|l| l == "+zz"), "{:?}", hunks[z]);
        fs::remove_dir_all(&repo).unwrap();
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
    fn marking_findings_to_fix() {
        let (mut app, _) = app();
        let state = std::env::temp_dir().join(format!("yogan-mark-{}", std::process::id()));
        sample_findings().save(&state, "t1").unwrap();
        app.state = state.clone();
        (app.detail, app.tab) = (true, FINDINGS);
        app.load_tab();
        let press = |app: &mut App, c| app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        press(&mut app, ' ');
        press(&mut app, 'j');
        press(&mut app, 'j');
        press(&mut app, ' ');
        press(&mut app, 'k');
        press(&mut app, ' ');
        press(&mut app, ' ');
        assert_eq!(app.marked, BTreeSet::from([0, 2]));
        let mut term = Terminal::new(TestBackend::new(60, 32)).unwrap();
        let theme = Theme::new(false, false);
        term.draw(|f| draw(f, &app, &theme, 0, SystemTime::UNIX_EPOCH))
            .unwrap();
        let rows = screen(&term);
        let row = |claim| rows.iter().find(|r| r.contains(claim)).unwrap().clone();
        assert!(row("-1 still parses").starts_with("│  ✓✗ src/config.rs:41"));
        assert!(row("no test for zero").starts_with("│ ▌ ◆ src/retry.rs:9"));
        assert!(row("may overflow").starts_with("│  ✓◆ src/lib.rs:3"));

        // only a task in Review takes findings back
        app.tasks[0].0.status = Status::Running;
        for c in ['f', 'F'] {
            app.notice = None;
            press(&mut app, c);
            assert!(app.notice.as_deref().unwrap().contains("in Review"));
        }
        assert_eq!(Findings::load(&state, "t1").unwrap(), sample_findings());
        assert_eq!(app.marked, BTreeSet::from([0, 2]));

        // in Review, f moves the marked ones to Fixed; the missing repo stops the worker starting
        app.tasks[0].0.status = Status::Review;
        app.repo = state.join("no-repo");
        press(&mut app, 'f');
        let saved = Findings::load(&state, "t1").unwrap();
        let claims =
            |fs: &[crate::critic::Finding]| fs.iter().map(|f| f.claim.clone()).collect::<Vec<_>>();
        assert_eq!(claims(&saved.fixed), ["-1 still parses", "may overflow"]);
        assert_eq!(claims(&saved.disputed), ["no test for zero"]);
        app.load_tab();
        assert!(app.marked.is_empty());
        // with none marked, f sends the one under the cursor
        sample_findings().save(&state, "t1").unwrap();
        app.load_tab();
        app.finding = 1;
        press(&mut app, 'f');
        let saved = Findings::load(&state, "t1").unwrap();
        assert_eq!(claims(&saved.fixed), ["no test for zero"]);
        // F sends every open, disputed and optional one
        sample_findings().save(&state, "t1").unwrap();
        press(&mut app, 'F');
        let saved = Findings::load(&state, "t1").unwrap();
        assert_eq!(saved.actionable().count(), 0);
        let all = ["-1 still parses", "no test for zero", "may overflow"];
        assert_eq!(claims(&saved.fixed), all);
        assert_eq!(saved.waived, sample_findings().waived);

        let prompt = send_prompt(&saved.fixed[..2]);
        assert!(prompt.contains(
            "1. [Blocker] src/config.rs:41 - -1 still parses\n   Evidence: cargo test negative fails"
        ));
        assert!(prompt.contains("2. [Major] src/retry.rs:9 - no test for zero"));
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
                " yogan   ● 2    enter  push and open PR  g  regenerate  e  e",
            ]
        );
    }

    #[test]
    fn markdown_lines() {
        let theme = Theme::new(false, false);
        let text = |l: &Line| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        };
        let md = markdown("use **fast** mode", &theme);
        assert_eq!(md.len(), 1);
        assert_eq!(text(&md[0]), "use fast mode");
        assert_eq!(md[0].spans[1], Span::raw("fast").bold());

        let md = markdown("run `cargo test` now", &theme);
        assert_eq!(text(&md[0]), "run cargo test now");
        assert_eq!(md[0].spans[1], Span::styled("cargo test", theme.shell));
        assert_eq!(
            markdown("snake_case_name", &theme),
            [Line::raw("snake_case_name")]
        );
        assert_eq!(
            markdown("```\n**x** `y` _z_\n```", &theme),
            [Line::styled("  **x** `y` _z_", theme.shell)]
        );

        let md = markdown("*it* and __b__ [docs](http://x)", &theme);
        assert_eq!(text(&md[0]), "it and b docs http://x");
        assert_eq!(md[0].spans[0], Span::raw("it").italic());
        assert_eq!(md[0].spans[2], Span::raw("b").bold());
        assert_eq!(md[0].spans[4], Span::raw("docs").underlined());
        assert_eq!(md[0].spans[5], Span::raw(" http://x").dim());
        let md = markdown("see [1] or [docs](u)", &theme);
        assert_eq!(text(&md[0]), "see [1] or docs u");
        assert_eq!(md[0].spans[1], Span::raw("docs").underlined());

        let md = markdown("- item\n  - sub\n1. **one**", &theme);
        let texts: Vec<String> = md.iter().map(text).collect();
        assert_eq!(texts, ["• item", "  • sub", "1. one"]);
        let ascii = markdown("- item", &Theme::new(false, true));
        assert_eq!(text(&ascii[0]), "- item");

        let md = markdown("> quoted\n---", &theme);
        assert_eq!(text(&md[0]), "│ quoted");
        assert_eq!(md[0].style, Style::new().dim());
        assert_eq!(md[1], Line::raw("─".repeat(8)).dim());

        let md = markdown("| a | bb |\n|---|---|\n| ccc | d |", &theme);
        let texts: Vec<String> = md.iter().map(text).collect();
        assert_eq!(texts, ["a    bb", "───────", "ccc  d"]);
        assert_eq!(md[0].style, Style::new().bold());
        assert_eq!(md[2].style, Style::new());

        assert_eq!(markdown("# Title", &theme), [Line::raw("Title").bold()]);
    }

    #[test]
    fn activity_from_stream() {
        let log = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/stream.jsonl");
        let calls = activity(&log, Path::new("/tmp/fixture"));
        let text = "I read `note.txt` (it says \"hello from the fixture\"), but I couldn't create \
            `out.txt`: the Write tool was denied permission for that path, so you'll need to grant \
            write access or create the file yourself.";
        assert_eq!(
            calls,
            [
                Act {
                    ok: Some(true),
                    ..act("Read", "note.txt")
                },
                Act {
                    ok: Some(false),
                    ..act("Write", "out.txt")
                },
                act(TEXT, text),
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
                act(NUDGE, "stalled for 15m"),
                act(HANDOFF, "context at 160000 of 200000 tokens")
            ]
        );
        fs::remove_file(&log).unwrap();
    }

    #[test]
    fn activity_timeline() {
        let (mut app, now) = verdict_app("running");
        app.tab = 1;
        let call = |id: &str, name: &str, input: &str| {
            format!(
                r#"{{"type":"assistant","message":{{"id":"m{id}","usage":{{}},"content":[{{"type":"tool_use","id":"{id}","name":"{name}","input":{input}}}]}}}}"#
            )
        };
        let result = |id: &str, error: bool, content: &str| {
            format!(
                r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"{id}","is_error":{error},"content":"{content}"}}]}}}}"#
            )
        };
        let text = |t: &str| {
            format!(
                r#"{{"type":"assistant","message":{{"id":"t","usage":{{}},"content":[{{"type":"text","text":"{t}"}}]}}}}"#
            )
        };
        let log = std::env::temp_dir().join(format!("yogan-timeline-{}.jsonl", std::process::id()));
        let lines = [
            call(
                "a",
                "Edit",
                r#"{"file_path":"/s/src/retry.rs","old_string":"a\nb","new_string":"a\nb\nc"}"#,
            ),
            result("a", false, "ok"),
            call("b", "Bash", r#"{"command":"cargo test -p webhook"}"#),
            result("b", true, r"Exit code 101\npanicked"),
            text(
                "The backoff test expects 3 attempts and the new cap makes it 2. Updating the test.",
            ),
            call("c", "Bash", r#"{"command":"cargo test -p webhook"}"#),
            result("c", false, "ok"),
            call("d", "Read", r#"{"file_path":"/s/src/mod.rs"}"#),
        ];
        fs::write(&log, lines.join("\n")).unwrap();
        app.activity = activity(&log, Path::new("/s"));
        fs::remove_file(&log).unwrap();
        let mut term = Terminal::new(TestBackend::new(60, 24)).unwrap();
        term.draw(|f| draw(f, &app, &Theme::new(false, false), 0, now))
            .unwrap();
        assert_eq!(
            screen(&term)[1..18],
            [
                "╭ Task ──────────────────────────────────────────── z zoom ╮",
                "│ plan ━ queue ━ ⠋ run ┄ check ┄ review ┄ pr               │",
                "│                                                          │",
                "│ context ━━━━━━━─ 85%        quiet   8m                   │",
                "│ slot 2 · opus                                            │",
                "│ spend   ━━━───── $1.84/$5.00                             │",
                "│                                                          │",
                "│ NEXT  Nothing needed yet.                                │",
                "│                                                          │",
                "│  Summary  Activity ⠋  Gate  Findings  Diff  Run          │",
                "│                                                          │",
                "│ edit    src/retry.rs                            +3 -2  ✓ │",
                "│ run     cargo test -p webhook                 ✗ exit 101 │",
                "│   │ The backoff test expects 3 attempts and the new cap  │",
                "│   │ makes it 2. Updating the test.                       │",
                "│ run     cargo test -p webhook                          ✓ │",
                "│ read    src/mod.rs                                       │",
            ]
        );
    }

    #[test]
    fn main_screen() {
        let (mut app, now) = app();
        app.tasks[2].0.slot = Some(2);
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        let theme = Theme::new(false, false);
        term.draw(|f| draw(f, &app, &theme, 0, now)).unwrap();
        assert_eq!(
            screen(&term),
            [
                " yogan · fuse-os   ● 2 need you   ⠋ 1 working  ○ 1 queued                         slots ▰▰▱   $0.00 ",
                "╭ Tasks ────────────────────────────────────╮╭ Task ─────────────────────────────────────── z zoom ╮",
                "│ ▾ NEEDS YOU 2 ─────────────────────────── ││ plan ━ queue ━ run ━ check ━ ◉ review ┄ pr          │",
                "│ ▌ ✓ Reject negative max_delay   ready  4m ││                                                     │",
                "│   ✗ Bump sqlx to 0.9           failed  1h ││ gate    ● 1/1            slot 1 · opus/high         │",
                "│                                           ││ spend   ──────── $0.00/$5.00                        │",
                "│ ▾ WORKING 1 ───────────────────────────── ││                                                     │",
                "│   ⠋ Retry webhook sends     working · 12m ││ NEXT   m  open the PR    r  ask for changes         │",
                "│                                           ││                                                     │",
                "│ ▾ LATER 1 ─────────────────────────────── ││  Summary  Activity  Gate ✓  Findings  Diff  Run     │",
                "│   ○ Split the ledger reconc… needs a slot ││                                                     │",
                "│                                           ││ Reject negative max_delay                           │",
                "│                                           ││ Review · u/reject-negative · slot 1 · opus/high     │",
                "│                                           ││                                                     │",
                "│                                           ││ max_delay below zero now fails at parse time.       │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "╰───────────────────────────────────────────╯╰─────────────────────────────────────────────────────╯",
                " m  open PR  M  merge to main  r  reply to worker  d  diff  x  discard task  c  continue  ?  more   ",
            ]
        );
    }

    #[test]
    fn a_heading_folds_and_a_slot_selects() {
        let (mut app, now) = app();
        app.tasks[2].0.slot = Some(2);
        let theme = Theme::new(false, false);
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        // draws, then clicks cell `nth` of `text`; returns the screen it clicked
        let mut click = |app: &mut App, text: &str, nth: usize| {
            term.draw(|f| draw(f, app, &theme, 0, now)).unwrap();
            let rows = screen(&term);
            let (y, row) = rows
                .iter()
                .enumerate()
                .find(|(_, r)| r.contains(text))
                .unwrap();
            let x = row[..row.find(text).unwrap()].chars().count() + nth;
            app.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: x as u16,
                row: y as u16,
                modifiers: KeyModifiers::NONE,
            });
            term.draw(|f| draw(f, app, &theme, 0, now)).unwrap();
            screen(&term)
        };
        let rows = click(&mut app, "NEEDS YOU", 0);
        assert!(rows.iter().any(|r| r.contains("▸ NEEDS YOU 2")));
        assert!(!rows.iter().any(|r| r.contains("Bump sqlx")));
        // folding the selection's group moves the selection to a shown row
        assert!(!app.groups()[0].contains(&app.selected));
        // k skips the folded group
        app.selected = 2;
        app.key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(app.selected, 2);
        let rows = click(&mut app, "NEEDS YOU", 0);
        assert!(rows.iter().any(|r| r.contains("Bump sqlx")));

        // the second slot holds the running task
        click(&mut app, "slots ▰▰▱", 7);
        assert_eq!(app.task().unwrap().0.id, "t3");
        click(&mut app, "slots ▰▰▱", 6);
        assert_eq!(app.task().unwrap().0.id, "t1");
    }

    #[test]
    fn next_for_you_wraps_within_needs_you() {
        let (mut app, _) = app();
        let next = |app: &mut App| {
            app.key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE));
            app.selected
        };
        // t1 and t2 need you; t3 is working and t4 is queued
        app.selected = 2;
        assert_eq!(next(&mut app), 0);
        assert_eq!(next(&mut app), 1);
        assert_eq!(next(&mut app), 0);
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
                    " m  open PR  M  merge to main  r  reply to worker  d  diff  ?  more             ".into(),
                    " m  open PR  M  merge to main  r  reply to worker  d  diff  x  discard task  c  continue  ?  more   ".into(),
                ),
                (
                    "failed",
                    " t  retry  c  continue  x  discard task  n  new task  1-6  tabs  ?  more        ".into(),
                    " t  retry  c  continue  x  discard task  n  new task  1-6  tabs  j/k  move  ?  more                 ".into(),
                ),
                (
                    "proposed",
                    " a  approve  A  approve all  e  edit  r  reply to lead  ?  more                 ".into(),
                    " a  approve  A  approve all  e  edit  r  reply to lead  x  discard task  n  new task  ?  more       ".into(),
                ),
                (
                    "question",
                    " p  plan it  r  reply to lead  y  copy  x  discard  n  new task  ?  more        ".into(),
                    " p  plan it  r  reply to lead  y  copy  x  discard  n  new task  j/k  move  ?  more                 ".into(),
                ),
            ]
        );
    }

    #[test]
    fn compact_layout_below_24_rows() {
        let (mut app, now) = app();
        app.tab = 1;
        app.activity = vec![
            act("Read", "src/config.rs"),
            act("Bash", "cargo test -p ledger"),
        ];
        let mut term = Terminal::new(TestBackend::new(100, 12)).unwrap();
        let theme = Theme::new(false, false);
        term.draw(|f| draw(f, &app, &theme, 0, now)).unwrap();
        assert_eq!(
            screen(&term),
            [
                "╭ Tasks ────────────────────────────────────╮╭ Task ─────────────────────────────────────── z zoom ╮",
                "│ ▌ ✓ Reject negative max_delay   ready  4m ││ plan ━ queue ━ run ━ check ━ ◉ review ┄ pr          │",
                "│   ✗ Bump sqlx to 0.9           failed  1h ││ gate ● 1/1  ·  slot 1 · opus/high  ·  spend ─────── │",
                "│   ⠋ Retry webhook sends     working · 12m ││ NEXT   m  open the PR    r  ask for changes         │",
                "│   ○ Split the ledger reconc… needs a slot ││  Activity ▾                                         │",
                "│                                           ││ read    src/config.rs                               │",
                "│                                           ││ run     cargo test -p ledger                        │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "╰───────────────────────────────────────────╯╰─────────────────────────────────────────────────────╯",
                " yogan   ● 2    m  open PR  M  merge to main  r  reply to worker  d  diff  x  discard task  ?  more ",
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
        assert!(click(&mut app, "Gate ✓"));
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
        assert!(click(&mut app, " x  discard") && app.confirm.is_some());
        assert!(click(&mut app, " esc  Keep it") && app.confirm.is_none());
        assert!(click(&mut app, " x  discard") && app.confirm.is_some());
        let dismissed = click(&mut app, " y  Dismiss") && app.confirm.is_none();
        assert!(dismissed && app.notice.is_none());
        assert_eq!(lead::load(&state, "r2").unwrap().status, Phase::Dismissed);
        fs::remove_dir_all(&state).unwrap();
    }

    /// The selected task's states for the verdict: a ready Review, a failed gate, a Proposed
    /// task and a Running one past `handoff_at`.
    fn verdict_app(state: &str) -> (App, SystemTime) {
        let (mut app, now) = app();
        let check = |name: &str, passed| Check {
            name: name.into(),
            passed,
        };
        app.detail = true;
        match state {
            "review" => {
                let t = &mut app.tasks[0].0;
                let names = ["fmt", "clippy", "test", "doc", "sqlx", "deny"];
                t.gate = Some(names.iter().map(|n| check(n, true)).collect());
                t.usage = [("s1".into(), 1.2)].into();
                app.findings = Findings {
                    fixed: sample_findings().findings,
                    ..Default::default()
                };
                app.diff = vec![
                    ("src/config/retry.rs".into(), 31, 4),
                    ("tests/config.rs".into(), 9, 2),
                    ("CHANGELOG.md".into(), 2, 1),
                ];
            }
            "failed" => {
                app.selected = 1;
                let t = &mut app.tasks[1].0;
                t.gate = Some(vec![check("fmt", true), check("clippy", true), {
                    check("test", false)
                }]);
                t.model = Some("sonnet".into());
                t.usage = [("s1".into(), 2.1)].into();
            }
            "proposed" => {
                app.selected = 1;
                app.tasks[1].0.status = Status::Proposed;
                app.tasks[1].0.model = Some("opus".into());
            }
            _ => {
                app.selected = 2;
                let t = &mut app.tasks[2].0;
                (t.slot, t.model) = (Some(2), Some("opus".into()));
                t.usage = [("s1".into(), 1.84)].into();
                app.context = Some(170_000);
                let call = ("Bash".into(), "cargo test".into());
                app.steps = vec![("t3".into(), call, now - Duration::from_secs(8 * 60))];
            }
        }
        (app, now)
    }

    /// The detail pane's top rows, without the header line the list's tickets change.
    fn verdict_screen(app: &App, now: SystemTime, height: u16, theme: &Theme) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(60, height)).unwrap();
        term.draw(|f| draw(f, app, theme, 0, now)).unwrap();
        let skip = usize::from(height >= COMPACT);
        screen(&term)[skip..skip + 11].to_vec()
    }

    #[test]
    fn verdict_and_next() {
        let theme = Theme::new(false, false);
        let shots: Vec<_> = ["review", "failed", "proposed", "running"]
            .into_iter()
            .map(|s| {
                let (app, now) = verdict_app(s);
                (s, verdict_screen(&app, now, 24, &theme))
            })
            .collect();
        let shots: Vec<(&str, Vec<&str>)> = (shots.iter())
            .map(|(s, rows)| (*s, rows.iter().map(String::as_str).collect()))
            .collect();
        assert_eq!(
            shots,
            [
                (
                    "review",
                    vec![
                        "╭ Task ──────────────────────────────────────────── z zoom ╮",
                        "│ plan ━ queue ━ run ━ check ━ ◉ review ┄ pr               │",
                        "│                                                          │",
                        "│ gate    ●●●●●● 6/6          critic  0 open · 1 fixed     │",
                        "│ change  +42 -7 · 3 files    slot 1 · opus/high           │",
                        "│ spend   ━━────── $1.20/$5.00                             │",
                        "│                                                          │",
                        "│ NEXT   m  open the PR    r  ask for changes              │",
                        "│                                                          │",
                        "│  Summary  Activity  Gate ✓  Findings  Diff  Run          │",
                        "│                                                          │",
                    ]
                ),
                (
                    "failed",
                    vec![
                        "╭ Task ──────────────────────────────────────────── z zoom ╮",
                        "│ plan ━ queue ━ run ━ ✗ check ┄ review ┄ pr               │",
                        "│                                                          │",
                        "│ gate    ●●✗ 2/3             sonnet                       │",
                        "│ spend   ━━━───── $2.10/$5.00                             │",
                        "│                                                          │",
                        "│ NEXT   t  retry    c  continue                           │",
                        "│                                                          │",
                        "│  Summary  Activity  Gate ✗  Findings  Diff  Run          │",
                        "│                                                          │",
                        "│ Bump sqlx to 0.9                                         │",
                    ]
                ),
                (
                    "proposed",
                    vec![
                        "╭ Task ──────────────────────────────────────────── z zoom ╮",
                        "│ ◉ plan ┄ queue ┄ run ┄ check ┄ review ┄ pr               │",
                        "│                                                          │",
                        "│ opus                                                     │",
                        "│ spend   ──────── $0.00/$5.00                             │",
                        "│                                                          │",
                        "│ NEXT   a  approve    e  edit                             │",
                        "│                                                          │",
                        "│  Summary  Activity  Gate  Findings  Diff  Run            │",
                        "│                                                          │",
                        "│ Bump sqlx to 0.9                                         │",
                    ]
                ),
                (
                    "running",
                    vec![
                        "╭ Task ──────────────────────────────────────────── z zoom ╮",
                        "│ plan ━ queue ━ ⠋ run ┄ check ┄ review ┄ pr               │",
                        "│                                                          │",
                        "│ context ━━━━━━━─ 85%        quiet   8m                   │",
                        "│ slot 2 · opus                                            │",
                        "│ spend   ━━━───── $1.84/$5.00                             │",
                        "│                                                          │",
                        "│ NEXT  Nothing needed yet.                                │",
                        "│                                                          │",
                        "│  Summary  Activity ⠋  Gate  Findings  Diff  Run          │",
                        "│                                                          │",
                    ]
                ),
            ]
        );
        // compact and ASCII: one verdict line, only the active pill
        let (app, now) = verdict_app("failed");
        assert_eq!(
            verdict_screen(&app, now, 12, &Theme::new(false, true))[..6],
            [
                "╭ Task ──────────────────────────────────────────── z zoom ╮",
                "│ plan = queue = run = x check - review - pr               │",
                "│ gate **x 2/3  ·  sonnet  ·  spend ===----- $2.10/$5.00   │",
                "│ NEXT   t  retry    c  continue                           │",
                "│  Summary ▾                                               │",
                "│ Bump sqlx to 0.9                                         │",
            ]
        );
    }

    #[test]
    fn right_click_opens_the_actions_menu() {
        let theme = Theme::new(false, false);
        let failed = || {
            let (mut app, now) = app();
            app.tasks[1].0.slot = Some(2);
            (app, now)
        };
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        // draws, then sends `kind` at the first cell of `text`
        let mut send = |app: &mut App, now, kind, text: &str| {
            term.draw(|f| draw(f, app, &theme, 0, now)).unwrap();
            let rows = screen(&term);
            let hit = rows.iter().enumerate().find(|(_, r)| r.contains(text));
            let (y, row) = hit.unwrap_or_else(|| panic!("no {text:?} in {rows:#?}"));
            let x = row[..row.find(text).unwrap()].chars().count();
            let m = MouseEvent {
                kind,
                column: x as u16,
                row: y as u16,
                modifiers: KeyModifiers::NONE,
            };
            (app.mouse(m), rows)
        };
        let right = MouseEventKind::Down(MouseButton::Right);
        let left = MouseEventKind::Down(MouseButton::Left);
        let (mut app, now) = failed();
        send(&mut app, now, right, "Bump sqlx");
        assert_eq!((app.selected, app.menu.is_some()), (1, true));

        // the menu over the Failed task, then a click on `t` does what pressing it does
        let (_, rows) = send(&mut app, now, left, "t   retry");
        assert_eq!(
            rows[4..15],
            [
                "│ ▌ ✗ Bump sqlx to 0.9           failed  1h ││ slot 2                                              │",
                "│      ╭ Bump sqlx to 0.9 ────────╮         ││ spend   ──────── $0.00/$5.00                        │",
                "│ ▾ WOR│ t   retry                │──────── ││                                                     │",
                "│   ⠋ R│ c   continue             │ng · 12m ││ NEXT   t  retry    c  continue                      │",
                "│      │ x   discard task         │         ││                                                     │",
                "│ ▾ LAT│ d   diff                 │──────── ││  Summary  Activity  Gate  Findings  Diff  Run       │",
                "│   ○ S│ o   open                 │s a slot ││                                                     │",
                "│      │ ──────────────────────── │         ││ Bump sqlx to 0.9                                    │",
                "│      │ z   zoom                 │         ││ Failed · slot 2                                     │",
                "│      │ ]   next for you         │         ││                                                     │",
                "│      ╰──────────────────────────╯         ││                                                     │",
            ]
        );
        let (mut pressed, _) = failed();
        pressed.selected = 1;
        pressed.key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        assert_eq!(app.reply_for, pressed.reply_for);
        assert_eq!((app.reply_for, app.menu), (Some('t'), None));

        // another key closes it and acts; a click outside only closes it
        let (mut app, now) = failed();
        send(&mut app, now, right, "Bump sqlx");
        app.key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
        assert_eq!((app.menu, app.tab), (None, 2));
        send(&mut app, now, right, "Bump sqlx");
        send(&mut app, now, left, "NEXT");
        assert_eq!((app.menu, app.reply_for, app.selected), (None, None, 1));
    }

    #[test]
    fn clicking_next_is_its_key() {
        let theme = Theme::new(false, false);
        // draws, then clicks the first cell of `text` above the footer
        let click = |app: &mut App, now, text: &str| {
            let mut term = Terminal::new(TestBackend::new(60, 24)).unwrap();
            term.draw(|f| draw(f, app, &theme, 0, now)).unwrap();
            let rows = screen(&term);
            let hit = rows[..23]
                .iter()
                .enumerate()
                .find(|(_, r)| r.contains(text));
            let (y, row) = hit.unwrap_or_else(|| panic!("no {text:?} in {rows:#?}"));
            let x = row[..row.find(text).unwrap()].chars().count();
            app.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: x as u16,
                row: y as u16,
                modifiers: KeyModifiers::NONE,
            })
        };
        let press = |app: &mut App, c| app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        for (state, key, button) in [("failed", 't', " t  retry"), ("proposed", 'e', " e  edit")] {
            let (mut pressed, _) = verdict_app(state);
            press(&mut pressed, key);
            let (mut clicked, now) = verdict_app(state);
            assert!(click(&mut clicked, now, button));
            assert_eq!(
                (clicked.reply_for, clicked.edit),
                (pressed.reply_for, pressed.edit)
            );
            assert!(clicked.reply_for == Some('t') || clicked.edit, "{state}");
        }

        // the wheel over the tab pills cycles the tabs
        let (mut app, now) = verdict_app("review");
        click(&mut app, now, " Summary ");
        let y = app.hits.borrow().tabs.y;
        app.mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 30,
            row: y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.tab, RUN);
    }

    #[test]
    fn hover_shows_the_key() {
        let (mut app, now) = app();
        let theme = Theme::new(false, false);
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|f| draw(f, &app, &theme, 0, now)).unwrap();
        let at = |term: &Terminal<TestBackend>, text: &str| {
            let rows = screen(term);
            let (y, row) = rows
                .iter()
                .enumerate()
                .find(|(_, r)| r.contains(text))
                .unwrap();
            Position::new(
                row[..row.find(text).unwrap()].chars().count() as u16,
                y as u16,
            )
        };
        let border = |term: &Terminal<TestBackend>| screen(term)[22].clone();

        // a move over a tab sets the hint, one within it changes nothing, empty space clears it
        let gate = at(&term, "Gate ✓  Findings");
        assert!(app.hover(gate));
        assert!(!app.hover(Position {
            x: gate.x + 1,
            ..gate
        }));
        term.draw(|f| draw(f, &app, &theme, 0, now)).unwrap();
        assert!(border(&term).ends_with("─ 3 · Gate ╯"), "{}", border(&term));
        assert!(app.hover(Position::new(70, 8)));
        term.draw(|f| draw(f, &app, &theme, 0, now)).unwrap();
        assert!(!border(&term).contains('·'));

        // a footer key: tinted, and named in the bottom border above it
        let m = at(&term, " m  open PR");
        assert!(app.hover(m));
        term.draw(|f| draw(f, &app, &theme, 0, now)).unwrap();
        assert_eq!(
            screen(&term),
            [
                " yogan · fuse-os   ● 2 need you   ⠋ 1 working  ○ 1 queued                         slots ▰▱▱   $0.00 ",
                "╭ Tasks ────────────────────────────────────╮╭ Task ─────────────────────────────────────── z zoom ╮",
                "│ ▾ NEEDS YOU 2 ─────────────────────────── ││ plan ━ queue ━ run ━ check ━ ◉ review ┄ pr          │",
                "│ ▌ ✓ Reject negative max_delay   ready  4m ││                                                     │",
                "│   ✗ Bump sqlx to 0.9           failed  1h ││ gate    ● 1/1            slot 1 · opus/high         │",
                "│                                           ││ spend   ──────── $0.00/$5.00                        │",
                "│ ▾ WORKING 1 ───────────────────────────── ││                                                     │",
                "│   ⠋ Retry webhook sends     working · 12m ││ NEXT   m  open the PR    r  ask for changes         │",
                "│                                           ││                                                     │",
                "│ ▾ LATER 1 ─────────────────────────────── ││  Summary  Activity  Gate ✓  Findings  Diff  Run     │",
                "│   ○ Split the ledger reconc… needs a slot ││                                                     │",
                "│                                           ││ Reject negative max_delay                           │",
                "│                                           ││ Review · u/reject-negative · slot 1 · opus/high     │",
                "│                                           ││                                                     │",
                "│                                           ││ max_delay below zero now fails at parse time.       │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "╰───────────────────────────────────────────╯╰──────────────────────────────────────── m · open PR ╯",
                " m  open PR  M  merge to main  r  reply to worker  d  diff  x  discard task  c  continue  ?  more   ",
            ]
        );
        let buf = term.backend().buffer();
        assert_eq!(buf[m].bg, theme.hover);
        assert_eq!(buf[(m.x + 11, m.y)].bg, theme.hover);
        assert_ne!(buf[(m.x + 12, m.y)].bg, theme.hover);
    }

    #[test]
    fn compose_is_clickable() {
        let (mut app, now) = app();
        app.compose = Some(Compose::new(Mode::Auto, ""));
        let theme = Theme::new(false, false);
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        // draws, then hovers and clicks the first cell of `text`
        let mut click = |app: &mut App, text: &str| {
            term.draw(|f| draw(f, app, &theme, 0, now)).unwrap();
            let rows = screen(&term);
            let hit = rows.iter().enumerate().find(|(_, r)| r.contains(text));
            let (y, row) = hit.unwrap_or_else(|| panic!("no {text:?} in {rows:#?}"));
            let at = Position::new(
                row[..row.find(text).unwrap()].chars().count() as u16,
                y as u16,
            );
            app.hover(at);
            term.draw(|f| draw(f, app, &theme, 0, now)).unwrap();
            let tinted = term.backend().buffer()[at].bg == theme.hover;
            app.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: at.x,
                row: at.y,
                modifiers: KeyModifiers::NONE,
            });
            (tinted, screen(&term))
        };
        let focus = |app: &App| app.compose.as_ref().map(|c| (c.mode, c.focus));

        // a mode's hint goes in the Mode pane's bottom border
        let (tinted, rows) = click(&mut app, "Plan");
        assert!(
            tinted && rows[3].ends_with(" click · Plan mode ╯"),
            "{rows:#?}"
        );
        assert_eq!(focus(&app), Some((Mode::Plan, 2)));
        click(&mut app, "Ask");
        assert_eq!(focus(&app), Some((Mode::Ask, 2)));
        click(&mut app, "Auto");
        assert_eq!(focus(&app), Some((Mode::Auto, 2)));

        let (tinted, rows) = click(&mut app, "Ticket");
        assert!(
            tinted && rows[21].ends_with(" click · type the ticket ╯"),
            "{rows:#?}"
        );
        assert_eq!(focus(&app), Some((Mode::Auto, 1)));
        app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(app.compose.as_ref().unwrap().ticket.lines(), ["x"]);
        click(&mut app, "Request");
        assert_eq!(focus(&app), Some((Mode::Auto, 0)));
        app.key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert_eq!(app.compose.as_ref().unwrap().request.lines(), ["y"]);

        // Submit with an empty request
        app.compose = Some(Compose::new(Mode::Auto, ""));
        // Submit's hint goes in the Ticket pane's border above, leaving its own row clear
        let (tinted, rows) = click(&mut app, "[ Submit ]");
        assert!(
            tinted && rows[21].ends_with(" ctrl-s · submit ╯"),
            "{rows:#?}"
        );
        assert_eq!(rows[22].trim_end(), " [ Submit ]");
        assert!(app.compose.is_some());
        assert_eq!(app.notice.as_deref(), Some("write a request first"));
    }

    #[test]
    fn hovering_a_slot_names_its_task() {
        let (mut app, now) = app();
        let theme = Theme::new(false, false);
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|f| draw(f, &app, &theme, 0, now)).unwrap();
        let header = &screen(&term)[0];
        let x = header[..header.find("slots ▰▱▱").unwrap()].chars().count() as u16 + 6;
        let border = |term: &Terminal<TestBackend>| screen(term)[22].clone();

        assert!(app.hover(Position::new(x, 0)));
        term.draw(|f| draw(f, &app, &theme, 0, now)).unwrap();
        assert!(
            border(&term).contains(" slot 1 · Reject negative max_delay "),
            "{}",
            border(&term)
        );

        assert!(app.hover(Position::new(x + 1, 0)));
        term.draw(|f| draw(f, &app, &theme, 0, now)).unwrap();
        assert!(
            border(&term).contains(" slot 2 · free "),
            "{}",
            border(&term)
        );
    }

    #[test]
    fn double_click_zooms() {
        let (mut app, now) = app();
        let theme = Theme::new(false, false);
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|f| draw(f, &app, &theme, 0, now)).unwrap();
        let row = screen(&term)
            .iter()
            .position(|r| r.contains("Bump sqlx"))
            .unwrap() as u16;
        let press = |app: &mut App, code| app.key(KeyEvent::new(code, KeyModifiers::NONE));
        let down = |column, row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        app.mouse(down(5, row));
        assert!(!app.zoom);
        app.mouse(down(6, row)); // another cell: still one click each
        assert!(!app.zoom);
        app.mouse(down(6, row));
        assert!(app.zoom && app.selected == 1);

        // ←/→ cycle the tabs, wrapping
        press(&mut app, KeyCode::Left);
        assert_eq!(app.tab, 5);
        press(&mut app, KeyCode::Right);
        assert_eq!(app.tab, 0);
        app.selected = 0;
        term.draw(|f| draw(f, &app, &theme, 0, now)).unwrap();
        assert_eq!(
            screen(&term),
            [
                " yogan · fuse-os   ● 2 need you   ⠋ 1 working  ○ 1 queued                         slots ▰▱▱   $0.00 ",
                "╭ Task ──────────────────────────────────────────────────────────────────────────────── esc unzoom ╮",
                "│ plan ━ queue ━ run ━ check ━ ◉ review ┄ pr                                                       │",
                "│                                                                                                  │",
                "│ gate    ● 1/1                                   slot 1 · opus/high                               │",
                "│ spend   ──────── $0.00/$5.00                                                                     │",
                "│                                                                                                  │",
                "│ NEXT   m  open the PR    r  ask for changes                                                      │",
                "│                                                                                                  │",
                "│  Summary  Activity  Gate ✓  Findings  Diff  Run                                                  │",
                "│                                                                                                  │",
                "│ Reject negative max_delay                                                                        │",
                "│ Review · u/reject-negative · slot 1 · opus/high                                                  │",
                "│                                                                                                  │",
                "│ max_delay below zero now fails at parse time.                                                    │",
                "│                                                                                                  │",
                "│                                                                                                  │",
                "│                                                                                                  │",
                "│                                                                                                  │",
                "│                                                                                                  │",
                "│                                                                                                  │",
                "│                                                                                                  │",
                "╰──────────────────────────────────────────────────────────────────────────────────────────────────╯",
                " m  open PR  M  merge to main  r  reply to worker  d  diff  x  discard task  c  continue  ?  more   ",
            ]
        );
        // the border label unzooms; z zooms again and esc goes back
        let label = screen(&term)[1].find("esc unzoom").unwrap();
        app.mouse(down(screen(&term)[1][..label].chars().count() as u16, 1));
        assert!(!app.zoom);
        press(&mut app, KeyCode::Char('z'));
        assert!(app.zoom);
        press(&mut app, KeyCode::Esc);
        assert!(!app.zoom);
    }

    #[test]
    fn toasts() {
        let (mut app, now) = app();
        app.info = Some("copied the answer".into());
        app.notice = Some("this task has no worktree".into());
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        let theme = Theme::new(false, false);
        term.draw(|f| draw(f, &app, &theme, 0, now)).unwrap();
        let rows = screen(&term);
        assert_eq!(
            rows[16..],
            [
                "│                                           ││                               ╭───────────────────╮ │",
                "│                                           ││                               │ copied the answer │ │",
                "│                                           ││                               ╰───────────────────╯ │",
                "│                                           ││                       ╭───────────────────────────╮ │",
                "│                                           ││                       │ this task has no worktree │ │",
                "│                                           ││                       ╰───────────────────────────╯ │",
                "╰───────────────────────────────────────────╯╰─────────────────────────────────────────────────────╯",
                " m  open PR  M  merge to main  r  reply to worker  d  diff  x  discard task  c  continue  ?  more   ",
            ]
        );

        // info fades after 4 s, an error stays until esc
        let t0 = Instant::now();
        app.expire(t0);
        app.expire(t0 + Duration::from_millis(3900));
        assert!(app.info.is_some());
        app.expire(t0 + Duration::from_secs(4));
        assert!(app.info.is_none() && app.notice.is_some());
        assert!(app.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));
        assert!(app.notice.is_some());
        assert!(app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(app.notice.is_none());

        // a click on a toast dismisses it
        let click = |app: &mut App, term: &mut Terminal<TestBackend>, text: &str| {
            term.draw(|f| draw(f, app, &theme, 0, now)).unwrap();
            let rows = screen(term);
            let y = rows.iter().position(|r| r.contains(text)).unwrap();
            let x = rows[y][..rows[y].find(text).unwrap()].chars().count();
            app.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: x as u16,
                row: y as u16,
                modifiers: KeyModifiers::NONE,
            });
        };
        app.info = Some("retrying".into());
        click(&mut app, &mut term, "retrying");
        assert!(app.info.is_none());
        app.notice = Some("the worker made no commits".into());
        click(&mut app, &mut term, "the worker made");
        assert!(app.notice.is_none());
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
                "│   + Reject negative max_delay                 [ready] 4m │",
                "│   x Bump sqlx to 0.9                         [failed] 1h │",
                "│ > / Retry webhook sends                    working · 12m │",
                "│   o Split the ledger reconciliation job in~ needs a slot │",
                "│                                                          │",
                "╰──────────────────────────────────────────────────────────╯",
                " yogan  [* 2]   x  discard task  n  new task  ?  more       ",
            ]
        );
    }

    #[test]
    fn dragging_the_divider() {
        let (mut app, now) = app();
        let theme = Theme::new(false, false);
        let mut term = Terminal::new(TestBackend::new(120, 24)).unwrap();
        let mouse = |kind, column| MouseEvent {
            kind,
            column,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        let border = |term: &Terminal<TestBackend>| {
            let row: Vec<char> = screen(term)[1].chars().collect();
            row.windows(2).position(|w| w == ['╮', '╭']).unwrap()
        };
        term.draw(|f| draw(f, &app, &theme, 0, now)).unwrap();
        assert_eq!(border(&term), 44);

        // the detail pane's left border grabs; drags move the list's right border to the cursor
        app.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 45));
        app.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 59));
        assert_eq!(app.split, 60);
        term.draw(|f| draw(f, &app, &theme, 0, now)).unwrap();
        assert_eq!(border(&term), 59);
        app.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 2));
        assert_eq!(app.split, 30);
        app.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 110));
        assert_eq!(app.split, 70);

        // once released, a drag elsewhere leaves it be
        app.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 110));
        app.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 50));
        assert_eq!(app.split, 70);
    }

    #[test]
    fn activity_scrollbar() {
        let (mut app, _) = app();
        app.activity = (1..=20).map(|i| act("Read", &format!("f{i}.rs"))).collect();
        app.detail = true;
        app.tab = 1;
        let theme = Theme::new(false, false);
        let mut term = Terminal::new(TestBackend::new(60, 14)).unwrap();
        term.draw(|f| draw(f, &app, &theme, 0, SystemTime::UNIX_EPOCH))
            .unwrap();
        assert_eq!(
            screen(&term),
            [
                "╭ Task ──────────────────────────────────────────── z zoom ╮",
                "│ plan ━ queue ━ run ━ check ━ ◉ review ┄ pr               │",
                "│ gate ● 1/1  ·  slot 1 · opus/high  ·  spend ──────── $0. │",
                "│ NEXT   m  open the PR    r  ask for changes              │",
                "│  Activity ▾                                              │",
                "│ read    f14.rs                                           │",
                "│ read    f15.rs                                           │",
                "│ read    f16.rs                                           │",
                "│ read    f17.rs                                           │",
                "│ read    f18.rs                                           │",
                "│ read    f19.rs                                           ┃",
                "│ read    f20.rs                                           ┃",
                "╰──────────────────────────────────────────────────────────╯",
                " yogan   ● 2    m  open PR  M  merge to main  ?  more       ",
            ]
        );

        // clicking the scrollbar's top jumps to the oldest calls
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 59,
            row: 5,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.scroll.get(), 13);
        term.draw(|f| draw(f, &app, &theme, 0, SystemTime::UNIX_EPOCH))
            .unwrap();
        assert!(screen(&term)[5].contains("f1.rs"));
    }

    #[test]
    fn palette_filters_and_runs_the_selected_row() {
        let (mut app, _) = app();
        app.tasks[1].0.status = Status::Proposed;
        app.selected = 1;
        let typed = |app: &mut App, text: &str| {
            for c in text.chars() {
                app.key(KeyEvent::from(KeyCode::Char(c)));
            }
        };
        typed(&mut app, "?appr");
        let labels = |app: &App| app.commands().into_iter().map(|c| c.1).collect::<Vec<_>>();
        assert_eq!(labels(&app), ["approve", "approve all"]);
        // `e edit` comes first among the rows `ed` keeps; enter runs it and closes the palette
        app.key(KeyEvent::from(KeyCode::Esc));
        typed(&mut app, ":ed");
        assert_eq!(labels(&app)[0], "edit");
        app.key(KeyEvent::from(KeyCode::Down));
        app.key(KeyEvent::from(KeyCode::Up));
        app.key(KeyEvent::from(KeyCode::Enter));
        assert!(app.edit && app.palette.is_none());
        // a `Go to` row selects its task
        typed(&mut app, "?go bump");
        app.key(KeyEvent::from(KeyCode::Backspace));
        typed(&mut app, "p");
        app.key(KeyEvent::from(KeyCode::Enter));
        assert_eq!((app.selected, app.palette.is_none()), (1, true));
    }

    #[test]
    fn palette_snapshot() {
        let (mut app, _) = app();
        app.tasks[1].0.status = Status::Proposed;
        app.selected = 1;
        app.palette = Some(("appr".into(), 0));
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        let theme = Theme::new(false, false);
        term.draw(|f| draw(f, &app, &theme, 0, SystemTime::UNIX_EPOCH))
            .unwrap();
        assert_eq!(
            screen(&term),
            [
                " yogan · fuse-os   ● 2 need you   ⠋ 1 working  ○ 1 queued                         slots ▰▱▱   $0.00 ",
                "╭ Tasks ────────────────────────────────────╮╭ Task ─────────────────────────────────────── z zoom ╮",
                "│ ▾ NEEDS YOU 2 ─────────────────────────── ││ ◉ plan ┄ queue ┄ run ┄ check ┄ review ┄ pr          │",
                "│   ✓ Reject negative max_delay      ready  ││                                                     │",
                "│ ▌ ○ Bump sqlx to 0.9        1 to approve  ││ spend   ──────── $0.00/$5.00                        │",
                "│                                           ││                                                     │",
                "│ ▾ WORKING 1 ───────────────────────────── ││ NEXT   a  approve    e  edit                        │",
                "│   ⠋ Retry webhook sends        working ·  ││                                                     │",
                "│                                           ││  Summary  Activity  Gate  Findings  Diff  Run       │",
                "│ ▾ LATER 1 ────╭ Commands ────────────────────────────────────────────────────────╮               │",
                "│   ○ Split the │ › appr▏                              type to filter · ↑↓ · enter │               │",
                "│               │ ──────────────────────────────────────────────────────────────── │               │",
                "│               │ ▌ a    approve                                  Bump sqlx to 0.9 │               │",
                "│               │   A    approve all                              Bump sqlx to 0.9 │               │",
                "│               ╰──────────────────────────────────────────────────────────────────╯               │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "╰───────────────────────────────────────────╯╰─────────────────────────────────────────────────────╯",
                " a  approve  A  approve all  e  edit  r  reply to lead  x  discard task  n  new task  ?  more       ",
            ]
        );
    }
}
