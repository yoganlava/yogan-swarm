//! The lead: one read-only Claude session per request, filing tasks with `yogan task propose`.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::redact::redact;
use crate::stream::{Event, System};
use crate::{config, task, worker};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Planning,
    Done,
    Failed,
}

/// One compose submission, saved as `requests/<id>.toml`.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub text: String,
    /// Carried to every task the lead files for it.
    pub ticket: Option<String>,
    pub status: Phase,
    pub session: Option<String>,
    /// The lead's final message, or why it failed.
    pub summary: Option<String>,
}

const RULES: &str = "\
You are yogan's lead. Turn the request into proposed tasks for unattended workers; never edit code.
- Each task fits in one session, stays under about 400 changed lines and 5 files, is mostly \
independent of the others, and names the crates it touches.
- Unrelated fixes become separate tasks, and so separate PRs. Split large work into ordered \
phases, passing the previous phase's id as --parent.
- A database migration is always its own task, because CI requires the migration PR type and \
allows such a PR to touch only migration SQL and .sqlx files.
- File each task with `yogan task propose --title <title> [--parent <id>] [--crates a,b] \
--accept <criterion> [--accept <criterion>...] <body>`. Give 1 to 5 testable criteria written as \
observable behaviour (\"a negative max_delay is rejected at parse time\", not \"handle bad \
config\"). It prints the new task's id.
- End with a short summary of the plan.";

impl Request {
    pub fn save(&self, dir: &Path) -> Result<()> {
        task::write_toml(&dir.join("requests"), &self.id, self)
    }
}

pub fn load(dir: &Path, id: &str) -> Result<Request> {
    let path = dir.join(format!("requests/{id}.toml"));
    let text = fs::read_to_string(&path).with_context(|| path.display().to_string())?;
    Ok(toml::from_str(&text)?)
}

/// Saves `text` as a new request and starts `yogan lead <id>` detached in `repo`.
pub fn submit(repo: &Path, state: &Path, text: &str, ticket: Option<String>) -> Result<Request> {
    let mut id = task::now_id("r");
    while state.join(format!("requests/{id}.toml")).exists() {
        id.push('a'); // two submits in one second
    }
    let req = Request {
        id,
        text: text.into(),
        ticket,
        ..Default::default()
    };
    req.save(state)?;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.args(["lead", &req.id]).current_dir(repo);
    worker::detach(cmd)?;
    Ok(req)
}

/// The lead's main: records the session, then `Done` or `Failed` with the reason.
pub fn run(repo: &Path, id: &str) -> Result<()> {
    let state = task::state_dir(repo)?;
    let mut req = load(&state, id)?;
    let res = plan(repo, &state, &mut req);
    req.status = if res.is_ok() {
        Phase::Done
    } else {
        Phase::Failed
    };
    if let Err(e) = &res {
        req.summary = Some(format!("{e:#}"));
    }
    req.save(&state)?;
    res
}

fn plan(repo: &Path, state: &Path, req: &mut Request) -> Result<()> {
    let cfg = config::load(repo)?;
    let (lead, w) = (&cfg.lead, &cfg.worker);
    worker::check_effort(&lead.effort)?;
    let read = ["Read", "Grep", "Glob"].map(String::from);
    let propose = ["Bash(yogan task propose:*)".to_string()];
    let allowed = [&read[..], &w.read_tools[..], &propose].concat();
    // the lead calls this binary as `yogan`
    let exe = std::env::current_exe()?;
    let bin = exe.parent().context("yogan has no directory")?;
    let path = std::env::join_paths(std::iter::once(bin.to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))?;
    let logs = state.join("logs");
    fs::create_dir_all(&logs)?;
    let append = |name: &str| {
        let path = logs.join(format!("{}.{name}", req.id));
        File::options().create(true).append(true).open(path)
    };
    let (mut log, mut err_log) = (append("jsonl")?, append("stderr.log")?);

    let mut cmd = Command::new("claude");
    cmd.args([
        "-p",
        &req.text,
        "--output-format",
        "stream-json",
        "--verbose",
    ])
    .args(["--setting-sources", "project", "--strict-mcp-config"])
    .args(["--model", &lead.model, "--effort", &lead.effort]);
    if let Some(file) = &w.mcp_config {
        cmd.args(["--mcp-config", file]);
    }
    let mut claude = cmd
        .arg("--allowedTools")
        .args(&allowed)
        .args(["--disallowedTools", "Edit", "Write", "NotebookEdit"])
        .args(["--append-system-prompt", RULES])
        .current_dir(repo)
        .env_remove("CLAUDE_CODE_EFFORT_LEVEL") // it would override --effort
        .env("PATH", path)
        .env("YOGAN_DIR", state)
        .env("YOGAN_REQUEST", &req.id)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting claude")?;
    let stderr = claude.stderr.take().context("claude stderr")?;
    let stderr = std::thread::spawn(move || -> std::io::Result<()> {
        for line in BufReader::new(stderr).lines() {
            writeln!(err_log, "{}", redact(&line?))?;
        }
        Ok(())
    });
    let stdout = claude.stdout.take().context("claude stdout")?;
    for line in BufReader::new(stdout).lines() {
        let line = line?;
        writeln!(log, "{}", redact(&line))?;
        match serde_json::from_str(&line) {
            Ok(Event::System(System::Init { session_id, .. })) => {
                req.session = Some(session_id);
                req.save(state)?;
            }
            Ok(Event::Result(r)) => req.summary = Some(r.result).filter(|s| !s.is_empty()),
            _ => {}
        }
    }
    let status = claude.wait()?;
    let _ = stderr.join();
    ensure!(status.success(), "claude exited with {status}");
    Ok(())
}
