//! The TUI: tasks grouped by status on the left, the selected task on the right. It only reads
//! the state directory; workers run detached, so closing it changes nothing.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
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
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::task::{self, Status, Task};

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

const KEYS: [(&str, &str); 4] = [
    ("j/k", "move"),
    ("tab", "switch pane"),
    ("?", "help"),
    ("q", "quit"),
];

/// Every color and glyph, so a light-terminal or ASCII variant is one swap. Nothing paints a
/// background: the terminal's own theme shows through.
pub struct Theme {
    accent: Color,
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
    name: String,
    /// Shown tasks in list order, each with when its file last changed.
    tasks: Vec<(Task, Option<SystemTime>)>,
    selected: usize,
    /// Narrow layout shows the detail pane instead of the list.
    detail: bool,
    help: bool,
}

pub fn run(repo: &Path) -> Result<()> {
    let state = task::state_dir(repo)?;
    reap(&state)?;
    let name = state.file_name().unwrap_or_default().to_string_lossy();
    let mut app = App {
        name: name.into_owned(),
        tasks: Vec::new(),
        selected: 0,
        detail: false,
        help: false,
    };
    let theme = Theme::detect();
    let mut terminal = ratatui::init();
    let start = Instant::now();
    let res = (|| -> Result<()> {
        loop {
            app.reload(&state)?;
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
        let ctrl_c =
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
        if key.code == KeyCode::Char('q') || ctrl_c {
            return false;
        }
        if self.help {
            self.help = false;
            return true;
        }
        match key.code {
            KeyCode::Char('?') => self.help = true,
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

fn draw(f: &mut Frame, app: &App, theme: &Theme, tick: usize, now: SystemTime) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(f.area());
    f.render_widget(header_line(app, theme), header);
    if body.width >= 100 {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(45), Constraint::Fill(1)]).areas(body);
        list(f, left, app, theme, !app.detail, tick, now);
        detail(f, right, app, theme, app.detail);
    } else if app.detail {
        detail(f, body, app, theme, true);
    } else {
        list(f, body, app, theme, true, tick, now);
    }
    let keys = KEYS.iter().flat_map(|(key, label)| {
        [
            Span::styled(format!(" {key} "), theme.accent),
            Span::raw(format!("{label} ")).dim(),
        ]
    });
    f.render_widget(Line::from(keys.collect::<Vec<_>>()), footer);
    if app.help {
        help(f, theme);
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
    let Some((t, _)) = app.tasks.get(app.selected) else {
        f.render_widget(
            Paragraph::new(Line::raw("No tasks yet.").dim()).block(block),
            area,
        );
        return;
    };
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
    let mut lines = vec![
        Line::raw(t.title.clone()).bold(),
        Line::raw(meta.join(" · ")).dim(),
        Line::raw(""),
    ];
    if let Some(summary) = &t.summary {
        lines.extend(summary.lines().map(|l| Line::raw(l.to_string())));
        lines.push(Line::raw(""));
    }
    lines.extend(t.body.lines().map(|l| Line::raw(l.to_string())));
    let text = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(text.block(block), area);
}

fn help(f: &mut Frame, theme: &Theme) {
    let width = 34.min(f.area().width);
    let height = (KEYS.len() as u16 + 2).min(f.area().height);
    let area = Rect {
        x: (f.area().width - width) / 2,
        y: (f.area().height - height) / 2,
        width,
        height,
    };
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
            name: "fuse-os".into(),
            tasks,
            selected: 0,
            detail: false,
            help: false,
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
                "│ Review                                    ││ Reject negative max_delay                           │",
                "│ ▌ ✓ Reject negative max_delay          4m ││ Review · u/reject-negative · slot 1 · opus/high     │",
                "│                                           ││                                                     │",
                "│ Failed                                    ││ max_delay below zero now fails at parse time.       │",
                "│   ✗ Bump sqlx to 0.9                   1h ││                                                     │",
                "│                                           ││                                                     │",
                "│ Running                                   ││                                                     │",
                "│   ⠋ Retry webhook sends     working · 12m ││                                                     │",
                "│                                           ││                                                     │",
                "│ Queued                                    ││                                                     │",
                "│   ○ Split the ledger reconciliation j… 2m ││                                                     │",
                "│                                           ││                                                     │",
                "╰───────────────────────────────────────────╯╰─────────────────────────────────────────────────────╯",
                " j/k move  tab switch pane  ? help  q quit                                                          ",
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
                " j/k move  tab switch pane  ? help  q quit                  ",
            ]
        );
    }
}
