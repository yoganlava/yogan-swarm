//! The lead: one read-only Claude session per request, filing tasks with `yogan task propose`.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::stream::{Event, System};
use crate::task::{self, Status};
use crate::{config, worker};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Planning,
    Done,
    Failed,
    /// A failed request the human set aside.
    Dismissed,
}

/// Auto lets the lead plan or answer; Plan always plans; Ask always answers.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Auto,
    Plan,
    Ask,
}

/// One compose submission, saved as `requests/<id>.toml`.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub mode: Mode,
    /// Carried to every task the lead files for it.
    pub ticket: Option<String>,
    pub status: Phase,
    pub session: Option<String>,
    /// The running lead's, for the TUI's dead-lead check.
    pub pid: Option<u32>,
    /// The lead's final message, or why it failed.
    pub summary: Option<String>,
    /// The follow-up being answered, so a retry asks it again.
    pub follow_up: Option<String>,
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

const AUTO: &str = "\
- If the request asks for information rather than a change, answer it instead: file no tasks, \
and write the answer in Markdown, citing the files it relies on as path:line.";

const ASK: &str = "\
You are yogan's lead, answering a question about this repo; never edit code. Answer in \
Markdown, citing the files you rely on as path:line.";

impl Request {
    pub fn save(&self, dir: &Path) -> Result<()> {
        task::write_toml(&dir.join("requests"), &self.id, self)
    }
}

pub fn load_all(dir: &Path) -> Result<Vec<Request>> {
    task::read_toml_dir(&dir.join("requests"))
}

pub fn load(dir: &Path, id: &str) -> Result<Request> {
    let path = dir.join(format!("requests/{id}.toml"));
    let text = fs::read_to_string(&path).with_context(|| path.display().to_string())?;
    Ok(toml::from_str(&text)?)
}

/// Saves `text` as a new request and starts `yogan lead <id>` detached in `repo`.
pub fn submit(
    repo: &Path,
    state: &Path,
    text: &str,
    ticket: Option<String>,
    mode: Mode,
) -> Result<Request> {
    if mode == Mode::Ask {
        check_room(repo, state)?;
    }
    let mut id = task::now_id("r");
    while state.join(format!("requests/{id}.toml")).exists() {
        id.push('a'); // two submits in one second
    }
    let req = Request {
        id,
        text: text.into(),
        ticket,
        mode,
        ..Default::default()
    };
    req.save(state)?;
    spawn(repo, state, &req.id, None)?;
    Ok(req)
}

/// Starts `yogan lead <id> [--reply <prompt>]` detached in `repo`. If it can't start, the
/// request fails with the reason, rather than staying in Planning with no lead.
pub fn spawn(repo: &Path, state: &Path, id: &str, reply: Option<&str>) -> Result<()> {
    let started = std::env::current_exe().and_then(|exe| {
        let mut cmd = Command::new(exe);
        cmd.args(["lead", id]).current_dir(repo);
        if let Some(prompt) = reply {
            cmd.args(["--reply", prompt]);
        }
        worker::detach(cmd).map_err(std::io::Error::other)
    });
    if let Err(e) = started {
        let mut req = load(state, id)?;
        req.status = Phase::Failed;
        req.summary = Some(format!("the lead didn't start: {e}"));
        req.save(state)?;
        return Err(e.into());
    }
    Ok(())
}

/// Back to Planning for another run of its lead.
fn restart(state: &Path, req: &mut Request) -> Result<()> {
    req.status = Phase::Planning;
    (req.summary, req.pid) = (None, None);
    req.save(state)
}

/// Readies request `id` for a reply: withdraws its pending proposals, since the lead refiles
/// the whole revised plan, and returns the prompt that resumes its session.
pub fn reply(state: &Path, id: &str, feedback: &str) -> Result<String> {
    let mut req = load(state, id)?;
    ensure!(req.status != Phase::Planning, "the lead is still planning");
    ensure!(req.session.is_some(), "the lead has no session to resume");
    let (mut withdrawn, mut kept) = (Vec::new(), Vec::new());
    for mut t in task::load_all(state)?.into_iter().filter(|t| t.plan == id) {
        let line = format!("- {} “{}”", t.id, t.title);
        match t.status {
            Status::Proposed => {
                t.status = Status::Discarded;
                t.save(state)?;
                withdrawn.push(line);
            }
            Status::Discarded => {}
            _ => kept.push(line),
        }
    }
    restart(state, &mut req)?;
    let mut prompt = format!(
        "{feedback}\n\nRevise the plan. Your pending proposals were withdrawn, so file every \
         task of the revised plan again with `yogan task propose`.\n\nWithdrawn:\n{}\n",
        withdrawn.join("\n")
    );
    if !kept.is_empty() {
        prompt.push_str(&format!(
            "\nAlready approved; don't refile these, but you can use them as --parent:\n{}\n",
            kept.join("\n")
        ));
    }
    Ok(prompt)
}

/// Errs when `[ask] concurrency` questions are already being answered.
fn check_room(repo: &Path, state: &Path) -> Result<()> {
    room(
        &load_all(state)?,
        config::load(repo)?.ask.concurrency as usize,
    )
}

fn room(requests: &[Request], limit: usize) -> Result<()> {
    let asking = requests
        .iter()
        .filter(|r| r.mode == Mode::Ask && r.status == Phase::Planning)
        .count();
    ensure!(
        asking < limit,
        "{asking} questions are being answered already ([ask] concurrency); try again shortly"
    );
    Ok(())
}

/// Readies answered request `id` to take `question`, which resumes its session.
pub fn follow_up(repo: &Path, state: &Path, id: &str, question: &str) -> Result<()> {
    let mut req = load(state, id)?;
    ensure!(
        req.status == Phase::Done && req.session.is_some(),
        "only an answered question takes a follow-up"
    );
    if req.mode == Mode::Ask {
        check_room(repo, state)?;
    }
    req.follow_up = Some(question.into());
    restart(state, &mut req)
}

/// Readies failed request `id` for another run. Returns the prompt that resumes its session,
/// or `None` to plan afresh when it never got one.
pub fn retry(state: &Path, id: &str) -> Result<Option<String>> {
    let mut req = load(state, id)?;
    ensure!(
        req.status == Phase::Failed,
        "only a failed request can be retried"
    );
    let why = req.summary.clone().unwrap_or_default();
    restart(state, &mut req)?;
    if req.follow_up.is_some() && req.session.is_some() {
        return Ok(req.follow_up);
    }
    // a question just asks again; the resume prompt is about filing tasks
    if req.session.is_none() || req.mode == Mode::Ask {
        return Ok(None);
    }
    let filed: Vec<_> = task::load_all(state)?
        .into_iter()
        .filter(|t| t.plan == id && t.status != Status::Discarded)
        .map(|t| format!("- {} “{}”", t.id, t.title))
        .collect();
    let mut prompt = format!(
        "Your last run stopped before finishing: {why}\n\nCarry on with the plan, filing the \
         tasks still missing with `yogan task propose`.\n"
    );
    if !filed.is_empty() {
        prompt.push_str(&format!(
            "\nAlready filed; don't refile these:\n{}\n",
            filed.join("\n")
        ));
    }
    Ok(Some(prompt))
}

/// The lead's main: plans the request, or resumes its session with a `reply`. Records the
/// session, then `Done` or `Failed` with the reason.
pub fn run(repo: &Path, id: &str, reply: Option<&str>) -> Result<()> {
    let state = task::state_dir(repo)?;
    let mut req = load(&state, id)?;
    req.pid = Some(std::process::id());
    req.save(&state)?;
    let res = plan(repo, &state, &mut req, reply);
    req.status = if res.is_ok() {
        Phase::Done
    } else {
        Phase::Failed
    };
    let asked = if res.is_ok() {
        req.follow_up.take()
    } else {
        None
    };
    if let Err(e) = &res {
        req.summary = Some(format!("{e:#}"));
    }
    req.save(&state)?;
    res?;
    save_answer(&state, &req, asked.as_deref())
}

pub fn answer_path(state: &Path, id: &str) -> PathBuf {
    state.join(format!("answers/{id}.md"))
}

/// Saves the lead's final message to `answers/<id>.md` when it answered rather than planned;
/// a follow-up's answer goes under its question.
fn save_answer(state: &Path, req: &Request, follow_up: Option<&str>) -> Result<()> {
    let planned = task::load_all(state)?.iter().any(|t| t.plan == req.id);
    let (Some(answer), false) = (&req.summary, planned || req.mode == Mode::Plan) else {
        return Ok(());
    };
    let path = answer_path(state, &req.id);
    fs::create_dir_all(state.join("answers"))?;
    match follow_up {
        Some(question) if path.exists() => {
            let mut file = File::options().append(true).open(&path)?;
            write!(file, "\n---\n\n**{}**\n\n{answer}\n", question.trim())?;
        }
        _ => fs::write(&path, format!("{answer}\n"))?,
    }
    Ok(())
}

fn plan(repo: &Path, state: &Path, req: &mut Request, reply: Option<&str>) -> Result<()> {
    let cfg = config::load(repo)?;
    let w = &cfg.worker;
    let ask = req.mode == Mode::Ask;
    let (model, effort) = match ask {
        true => (&cfg.ask.model, &cfg.ask.effort),
        false => (&cfg.lead.model, &cfg.lead.effort),
    };
    worker::check_effort(effort)?;
    let read = ["Read", "Grep", "Glob"].map(String::from);
    // a question researches instead of filing tasks
    let extra = match ask {
        true => vec!["WebSearch".to_string(), "WebFetch".to_string()],
        false => vec!["Bash(yogan task propose:*)".to_string()],
    };
    let allowed = [&read[..], &w.read_tools[..], &extra].concat();
    let mut deny = vec!["Edit", "Write", "NotebookEdit"];
    deny.extend(ask.then_some("Bash"));
    let rules = match req.mode {
        Mode::Ask => ASK.to_string(),
        Mode::Plan => RULES.to_string(),
        Mode::Auto => format!("{RULES}\n{AUTO}"),
    };
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
    let (mut log, err_log) = (append("jsonl")?, append("stderr.log")?);

    let mut cmd = Command::new("claude");
    cmd.args(["--model", model, "--effort", effort]);
    if let Some(file) = &w.mcp_config {
        cmd.args(["--mcp-config", file]);
    }
    if reply.is_some() {
        cmd.args([
            "--resume",
            req.session.as_deref().context("no session to resume")?,
        ]);
    }
    cmd.arg("--allowedTools")
        .args(&allowed)
        .arg("--disallowedTools")
        .args(&deny)
        .args(["--append-system-prompt", &rules])
        .current_dir(repo)
        .env("PATH", path)
        .env("YOGAN_DIR", state)
        .env("YOGAN_REQUEST", &req.id);
    let prompt = reply.map_or_else(|| req.text.clone(), String::from);
    worker::claude(&mut cmd, &prompt, &mut log, &err_log, |event| {
        match event {
            Event::System(System::Init { session_id, .. }) => {
                req.session = Some(session_id);
                req.save(state)?;
            }
            Event::Result(r) => req.summary = Some(r.result).filter(|s| !s.is_empty()),
            _ => {}
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn questions_have_their_own_limit() {
        let req = |mode, status| Request {
            mode,
            status,
            ..Default::default()
        };
        let mut requests = vec![
            req(Mode::Ask, Phase::Planning),
            req(Mode::Ask, Phase::Done),
            req(Mode::Plan, Phase::Planning),
        ];
        assert!(room(&requests, 2).is_ok());
        requests.push(req(Mode::Ask, Phase::Planning));
        let err = room(&requests, 2).unwrap_err().to_string();
        assert!(err.contains("[ask] concurrency"), "{err}");
    }
}
