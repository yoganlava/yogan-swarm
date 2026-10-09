//! `yogan worker <id>`: one task's whole lifecycle, run detached in its own session.

use std::collections::HashMap;
use std::fs::{self, File, TryLockError};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use rustix::io::Errno;
use rustix::process::{
    Pid, Resource, Rlimit, Signal, getrlimit, kill_process, kill_process_group, setrlimit, setsid,
    test_kill_process,
};
use serde_json::Value;
use sysinfo::{ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};

use crate::critic::{self, Findings};
use crate::redact::redact;
use crate::stream::{self, Content, Event, RunResult};
use crate::task::{self, Status, Task};
use crate::{config, gate, git, pr, sched, shim, slot};

pub(crate) const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// Built-in worker denies; a project's `[worker] deny` adds to them.
const DENY: &[&str] = &[
    "Bash(git push *)",
    "Bash(git tag *)",
    "Bash(git checkout *)",
    "Bash(git reset *)",
    "Bash(git stash drop *)",
    "Bash(git clean *)",
    "Bash(git worktree *)",
    "Bash(gh *)",
    "Bash(psql *)",
    "WebFetch",
];

const RULES: &str = "\
You are a yogan worker running unattended in your own git worktree.
- Stay within the task's crates and acceptance criteria.
- Use the least plumbing possible: extend existing endpoints, enums and structs, and add no new \
events, traits, endpoints or sub-enums unless a criterion requires them. If the change needs more \
plumbing than the task implies, stop and report instead of building it.
- Run the project's checks scoped to the touched crates or packages only (`cargo test -p`, never \
the workspace suite); CI covers the rest.
- Commit in semantic commits of roughly 300 lines or fewer, with conventional messages such as \
`fix(ledger): ...`. Never push or open a PR.
- Never run destructive git: `checkout --`, `reset --hard`, `stash drop`, `clean`, `push --force`.
- Change files with the Edit and Write tools, never with shell scripts (`python3 -`, `sed -i`, \
heredocs), and keep `${...}` out of commands: both are denied here. To check an exit status, run \
`cmd > /tmp/out.log 2>&1; echo exit=$?`.
- If YOGAN_PORT_BASE is set, your ports are YOGAN_PORT_BASE up to YOGAN_PORT_BASE + \
YOGAN_PORT_COUNT - 1; never bind any other.
- End with a plain summary that someone without context can follow, flagging anything you were \
unsure of.";

const WHOLE: &str = "End with a plain summary of the whole change on this branch so far, not \
only this step.";

/// Starts `yogan worker <id>` for the checkout `repo` in a new session, so neither Ctrl-C
/// nor a closing terminal reaches it. Returns its pid, which is also its process group.
pub fn spawn(repo: &Path, id: &str, extra: &[&str]) -> Result<u32> {
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.args(["worker", id]).args(extra).current_dir(repo);
    detach(cmd)
}

/// Runs `cmd`, a `claude` with its role's flags, as `claude -p` on `prompt`, which goes last
/// after `--` so one starting with `-` isn't read as an option. The event stream goes redacted
/// to `log` and parsed to `on_event`, whose error kills claude; stderr goes redacted to
/// `err_log`. With `watch` (the thresholds and the slot's permit queue), a stall or loop kills
/// it with a `Nudge` error.
pub(crate) fn claude(
    cmd: &mut Command,
    prompt: &str,
    log: &mut File,
    err_log: &File,
    watch: Option<(&config::Watch, &Path)>,
    mut on_event: impl FnMut(Event) -> Result<()>,
) -> Result<()> {
    let mut claude = cmd
        .args(["-p", "--output-format", "stream-json", "--verbose"])
        .args(["--setting-sources", "project", "--strict-mcp-config"])
        .args(["--", prompt])
        .env_remove("CLAUDE_CODE_EFFORT_LEVEL") // it would override --effort
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting claude")?;
    let pid = claude.id();
    let stderr = claude.stderr.take().context("claude stderr")?;
    let mut err_log = err_log.try_clone()?;
    let stderr = std::thread::spawn(move || -> std::io::Result<()> {
        for line in BufReader::new(stderr).lines() {
            writeln!(err_log, "{}", redact(&line?))?;
        }
        Ok(())
    });

    let idle = Arc::new(Mutex::new(Idle::new(Instant::now())));
    let stalled = Arc::new(AtomicBool::new(false));
    let (done, ticks) = mpsc::channel::<()>();
    let watcher = watch.map(|(w, queue)| {
        let (after, queue) = (w.stall_after, queue.to_path_buf());
        let (idle, stalled) = (idle.clone(), stalled.clone());
        std::thread::spawn(move || {
            let mut sys = System::new();
            while ticks.recv_timeout(TICK) == Err(RecvTimeoutError::Timeout) {
                refresh(&mut sys);
                let cpu = tree(&sys, pid)
                    .iter()
                    .filter_map(|p| sys.process(*p))
                    .map(|p| p.accumulated_cpu_time())
                    .sum();
                let tick = idle
                    .lock()
                    .unwrap()
                    .tick(Instant::now(), cpu, queued(&queue));
                if tick >= after {
                    stalled.store(true, Ordering::SeqCst);
                    kill_tree(pid);
                    return;
                }
            }
        })
    });

    let (mut repeats, mut last_call, mut last_result) = (Repeats::default(), None, None);
    let mut looped = None;
    let stdout = claude.stdout.take().context("claude stdout")?;
    for line in BufReader::new(stdout).lines() {
        let line = line?;
        idle.lock().unwrap().event(Instant::now());
        writeln!(log, "{}", redact(&line))?;
        let Ok(event) = serde_json::from_str::<Event>(&line) else {
            continue;
        };
        match &event {
            Event::Assistant { message } => {
                for c in &message.content {
                    if let Content::ToolUse { name, input, .. } = c {
                        last_call = Some(call(name, input));
                        last_result = None;
                        let n = repeats.see(name, input);
                        if let Some((w, _)) = watch
                            && n >= w.loop_repeats
                        {
                            looped = Some(n);
                        }
                    }
                }
            }
            Event::User { message } => last_result = tool_result(message).or(last_result),
            _ => {}
        }
        let res = on_event(event);
        if let (Ok(()), Some(n)) = (&res, looped) {
            let call = last_call.unwrap_or_default();
            kill_tree(pid);
            let _ = claude.wait();
            return Err(Nudge {
                reason: format!("repeated `{call}` {n} times"),
                prompt: format!(
                    "yogan stopped you: you made the same call `{call}` with the same input {n} \
                     times in a row. State a hypothesis for why it keeps giving the same result, \
                     then try a different approach."
                ),
            }
            .into());
        }
        if let Err(e) = res {
            kill_tree(pid);
            let _ = claude.wait();
            return Err(e);
        }
    }
    let status = claude.wait()?;
    drop(done);
    if let Some(w) = watcher {
        let _ = w.join();
    }
    let _ = stderr.join();
    if stalled.load(Ordering::SeqCst) {
        let mins = watch.map_or(0, |(w, _)| w.stall_after.as_secs() / 60);
        let last = match (&last_call, &last_result) {
            (Some(call), Some(result)) => {
                format!("Your last call was `{call}`, which returned:\n{result}")
            }
            (Some(call), None) => format!("Your last call was `{call}`, which never returned."),
            _ => "You hadn't made a tool call yet.".into(),
        };
        return Err(Nudge {
            reason: format!("stalled for {mins}m"),
            prompt: format!(
                "yogan stopped you: there was no output and no running work for {mins} minutes. \
                 {last}\n\nName the next concrete step and take it, or stop with a clear reason \
                 why you can't go on."
            ),
        }
        .into());
    }
    ensure!(status.success(), "claude exited with {status}");
    Ok(())
}

/// A watched run stopped for a stall or loop; `prompt` resumes it.
#[derive(Debug)]
pub(crate) struct Nudge {
    pub reason: String,
    pub prompt: String,
}

impl std::fmt::Display for Nudge {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for Nudge {}

const TICK: Duration = Duration::from_secs(10);
// ponytail: a fixed floor of 1% of a core per tick; claude idling on its own timers stays under
// it, so a run spinning just above it never stalls; measure claude's idle CPU if that bites
const BUSY_MS: u64 = 100;
const GRACE: Duration = Duration::from_secs(5);

/// How long the stream and the process tree have both been idle.
struct Idle {
    since: Instant,
    cpu_ms: u64,
}

impl Idle {
    fn new(now: Instant) -> Self {
        Idle {
            since: now,
            cpu_ms: 0,
        }
    }

    fn event(&mut self, now: Instant) {
        self.since = now;
    }

    /// `cpu_ms` is the tree's total CPU time; a build `queued` for a permit counts as busy.
    fn tick(&mut self, now: Instant, cpu_ms: u64, queued: bool) -> Duration {
        if queued || cpu_ms > self.cpu_ms + BUSY_MS {
            self.since = now;
        }
        self.cpu_ms = cpu_ms;
        now - self.since
    }
}

/// Counts the same tool call with the same input in a row.
#[derive(Default)]
struct Repeats {
    last: Option<(String, Value)>,
    n: u32,
}

impl Repeats {
    fn see(&mut self, name: &str, input: &Value) -> u32 {
        if self.last.as_ref() == Some(&(name.to_string(), input.clone())) {
            self.n += 1;
        } else {
            self.last = Some((name.to_string(), input.clone()));
            self.n = 1;
        }
        self.n
    }
}

/// A tool call as `Tool target`, for a nudge.
fn call(name: &str, input: &Value) -> String {
    let target = ["command", "file_path", "pattern"]
        .iter()
        .find_map(|k| input[k].as_str())
        .map_or_else(|| input.to_string(), str::to_string);
    format!("{name} {target}").chars().take(200).collect()
}

/// The tail of the first tool result in a user message.
fn tool_result(message: &Value) -> Option<String> {
    let blocks = message["content"].as_array()?;
    let block = blocks.iter().find(|b| b["type"] == "tool_result")?;
    let text = match &block["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => {
            let parts: Vec<_> = parts.iter().filter_map(|p| p["text"].as_str()).collect();
            parts.join("\n")
        }
        _ => String::new(),
    };
    let skip = text.chars().count().saturating_sub(2000);
    Some(text.chars().skip(skip).collect())
}

/// Whether a build holds the permit queue, i.e. waits for a permit.
fn queued(queue: &Path) -> bool {
    File::open(queue).is_ok_and(|f| matches!(f.try_lock(), Err(TryLockError::WouldBlock)))
}

fn refresh(sys: &mut System) {
    let kind = ProcessRefreshKind::nothing().with_cpu();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
}

/// `root` and its descendants.
fn tree(sys: &System, root: u32) -> Vec<sysinfo::Pid> {
    let mut kids: HashMap<sysinfo::Pid, Vec<sysinfo::Pid>> = HashMap::new();
    for (pid, p) in sys.processes() {
        if let Some(parent) = p.parent() {
            kids.entry(parent).or_default().push(*pid);
        }
    }
    let mut out = vec![sysinfo::Pid::from_u32(root)];
    let mut i = 0;
    while i < out.len() {
        out.extend(kids.get(&out[i]).cloned().unwrap_or_default());
        i += 1;
    }
    out
}

/// SIGTERMs `root` and its descendants, then SIGKILLs any left after a grace period. Walks the
/// tree, not a process group, since claude shares the worker's group.
fn kill_tree(root: u32) {
    let mut sys = System::new();
    refresh(&mut sys);
    let pids = tree(&sys, root);
    let signal = |s| {
        for p in pids.iter().filter_map(|p| Pid::from_raw(p.as_u32() as i32)) {
            let _ = kill_process(p, s);
        }
    };
    signal(Signal::TERM);
    let start = Instant::now();
    while start.elapsed() < GRACE {
        std::thread::sleep(Duration::from_millis(100));
        refresh(&mut sys);
        let gone = |p| {
            sys.process(p)
                .is_none_or(|p| p.status() == ProcessStatus::Zombie)
        };
        if pids.iter().all(|p| gone(*p)) {
            return;
        }
    }
    signal(Signal::KILL);
}

/// Claude only warns on an unknown effort and runs on its default, so yogan fails instead.
pub(crate) fn check_effort(effort: &str) -> Result<()> {
    ensure!(
        EFFORTS.contains(&effort),
        "unknown effort {effort:?}, expected one of {}",
        EFFORTS.join(", ")
    );
    Ok(())
}

/// Runs `cmd` in a new session with no stdio. Returns its pid, which is also its process group.
pub(crate) fn detach(mut cmd: Command) -> Result<u32> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe.
    unsafe { cmd.pre_exec(|| setsid().map(drop).map_err(Into::into)) };
    let mut child = cmd.spawn()?;
    let pid = child.id();
    // reap it when it exits, so a dead worker doesn't linger as a zombie that looks alive
    std::thread::spawn(move || child.wait());
    Ok(pid)
}

/// SIGTERMs a worker's whole process group, so builds holding permits die with it.
pub fn stop(pid: u32) -> Result<()> {
    let pid = Pid::from_raw(pid as i32).context("pid 0")?;
    Ok(kill_process_group(pid, Signal::TERM)?)
}

pub fn alive(pid: u32) -> bool {
    Pid::from_raw(pid as i32).is_some_and(|p| test_kill_process(p) != Err(Errno::SRCH))
}

/// The worker's main. With `pr` (an instruction, or empty), it rebases a task in Review and
/// drafts its PR instead of starting fresh; with `reply`, it resumes the task's session with it
/// and runs the gate and critic again. Any error marks the task `Failed`, with the error as its
/// summary. On exit it starts whatever is ready next.
pub fn run(repo: &Path, id: &str, pr: Option<&str>, reply: Option<&str>) -> Result<()> {
    let state = task::state_dir(repo)?;
    let mut task = task::load_all(&state)?
        .into_iter()
        .find(|t| t.id == id)
        .with_context(|| format!("no task {id}"))?;
    let res = lifecycle(repo, &state, &mut task, pr, reply);
    if let Err(e) = &res
        && (pr.is_some() || reply.is_some())
        && task.status == Status::Review
    {
        // the work is intact: the fetch, rebase or draft failed, or another worker holds the slot
        fs::create_dir_all(state.join("logs"))?;
        fs::write(state.join(format!("logs/{id}.pr.log")), format!("{e:#}\n"))?;
    } else if let Err(e) = &res {
        task.status = Status::Failed;
        // keep what the worker said it did under the error
        let said = task
            .summary
            .take()
            .map_or(String::new(), |s| format!("\n\n{s}"));
        task.summary = Some(format!("{e:#}{said}"));
        task.save(&state)?;
    }
    // this worker's slot lock is released by now, so the next task can take it
    let next = crate::sched::run(repo);
    res?;
    next
}

fn lifecycle(
    repo: &Path,
    state: &Path,
    task: &mut Task,
    pr: Option<&str>,
    reply: Option<&str>,
) -> Result<()> {
    // a task in Review keeps its slot and worktree
    let existing = pr.is_some() || reply.is_some();
    let cfg = config::load(repo)?;
    if let Some(nofile) = cfg.cargo.as_ref().and_then(|c| c.nofile) {
        let hard = getrlimit(Resource::Nofile).maximum;
        let limit = Rlimit {
            current: Some(nofile),
            maximum: hard,
        };
        setrlimit(Resource::Nofile, limit).context("[cargo] nofile")?;
    }
    let tasks = task::load_all(state)?;
    let (n, _lock) = match existing {
        true => {
            let n = task.slot.context("the task has no slot")?;
            (n, slot::lock(state, n)?)
        }
        false => slot::claim(state, &tasks, cfg.worker.slots)?.context("no free slot")?,
    };
    let model = task.model.clone().unwrap_or(cfg.worker.model.clone());
    let effort = task.effort.clone().unwrap_or(cfg.worker.effort.clone());
    check_effort(&effort)?;
    task.model = Some(model.clone());
    task.effort = Some(effort.clone());
    // --pr leaves the task in Review until something runs, so a failed fetch keeps it there
    if pr.is_none() {
        task.status = Status::Running;
    }
    task.slot = Some(n);
    task.pid = Some(std::process::id());
    task.save(state)?;

    let base = task::base(task, tasks.iter());
    let base = base.as_str();
    let dir = match existing {
        true => state.join("slots").join(n.to_string()),
        false => slot::prepare(repo, state, n, &task.branch, base, cfg.disk.max_target_gb)?,
    };
    let mut env = slot::env(repo, &dir, n, task, cfg.ports.as_ref());
    if repo.join("Cargo.toml").exists() {
        let home = std::env::home_dir().context("no home directory")?;
        let wrapper = cfg.cargo.as_ref().and_then(|c| c.wrapper.as_ref());
        env.extend(shim::env(
            state,
            n,
            &std::env::current_exe()?,
            &home.join(".local/share/yogan/permits"),
            cfg.build.max_cargo,
            wrapper.map(|w| dir.join(w)).as_deref(),
        )?);
        env.push(("CARGO_TARGET_DIR", dir.join("target").display().to_string()));
    }
    let logs = state.join("logs");
    let script = cfg.scripts.as_ref().and_then(|s| s.setup.as_deref());
    let setup_log = logs.join(format!("{}.setup.log", task.id));
    if !existing {
        slot::setup(repo, &dir, &env, script, &setup_log)?;
    }

    let w = &cfg.worker;
    ensure!(
        !w.allowed_tools.is_empty(),
        "[worker] allowed_tools is empty"
    );
    // a rebase conflict is the worker's to resolve
    let rebasing = ["Bash(git rebase *)".to_string()];
    let allowed = [&w.allowed_tools[..], &w.read_tools[..], &rebasing].concat();
    let servers = match &w.mcp_config {
        Some(file) => mcp_servers(&dir.join(file))?,
        None => Vec::new(),
    };
    fs::create_dir_all(&logs)?;
    let append = |name: &str| {
        let path = logs.join(format!("{}.{name}", task.id));
        File::options().create(true).append(true).open(path)
    };
    let (mut log, err_log) = (append("jsonl")?, append("stderr.log")?);

    // one Claude run in the slot; `resume` continues the task's last session, and a `schema`
    // asks for a structured reply, which it returns. A stall or loop resumes it with a nudge,
    // up to [watch] nudges per task; a full context, or a transcript Claude Code has deleted,
    // hands off to a fresh session
    let queue = shim::queue(state, n);
    let mut session = |task: &mut Task,
                       prompt: &str,
                       resume: bool,
                       schema: Option<&str>|
     -> Result<Option<Value>> {
        // the branch is about to change, so any PR draft is stale
        task.status = Status::Running;
        task.pr_draft = None;
        task.save(state)?;
        let (instruction, mut resume) = (prompt, resume);
        let mut prompt = prompt.to_string();
        let mut retried = false;
        loop {
            let mut cmd = Command::new("claude");
            cmd.args(["--permission-mode", "acceptEdits"])
                .args(["--model", &model, "--effort", &effort])
                .args(["--autocompact", &cfg.watch.autocompact.to_string()]);
            if let Some(file) = &w.mcp_config {
                cmd.args(["--mcp-config", file]);
            }
            if resume {
                cmd.args([
                    "--resume",
                    task.sessions.last().context("no session to resume")?,
                ]);
            }
            if let Some(schema) = schema {
                cmd.args(["--json-schema", schema]);
            }
            let ceiling = task.budget_usd.unwrap_or(w.budget_usd);
            if let Some(left) = budget_left(ceiling, task.spent())? {
                cmd.args(["--max-budget-usd", &left.to_string()]);
            }
            cmd.arg("--allowedTools")
                .args(&allowed)
                .arg("--disallowedTools")
                .args(DENY)
                .args(&w.deny)
                .args(["--append-system-prompt", RULES])
                .current_dir(&dir)
                .env("GIT_EDITOR", "true") // `rebase --continue` must not wait on an editor
                .envs(env.iter().cloned());
            let (mut denied, mut reply) = (Vec::new(), None);
            let (mut fresh, mut said) = (None, None);
            let (mut rejected, mut step, mut outcome) = (None, None, None);
            let watch = Some((&cfg.watch, queue.as_path()));
            // a resumed run's summary replaces the last one, so it has to cover everything
            let sent = match resume && schema.is_none() {
                true => format!("{prompt}\n\n{WHOLE}"),
                false => prompt.clone(),
            };
            let res = claude(&mut cmd, &sent, &mut log, &err_log, watch, |event| {
                match event {
                    Event::System(stream::System::Init {
                        session_id,
                        mcp_servers,
                        ..
                    }) => {
                        if !task.sessions.contains(&session_id) {
                            task.sessions.push(session_id);
                            task.save(state)?;
                        }
                        let extra: Vec<_> = mcp_servers
                            .into_iter()
                            .filter(|s| !servers.contains(&s.name))
                            .map(|s| s.name)
                            .collect();
                        ensure!(
                            extra.is_empty(),
                            "unexpected MCP servers: {}",
                            extra.join(", ")
                        );
                    }
                    Event::Assistant { message } => {
                        let text = message.content.iter().rev().find_map(|c| match c {
                            Content::Text { text } => Some(text.clone()),
                            _ => None,
                        });
                        said = text.or(said.take());
                        let mut calls = message.content.iter().filter_map(|c| match c {
                            Content::ToolUse { name, input, .. } => Some(call(name, input)),
                            _ => None,
                        });
                        step = calls.next_back().or(step.take());
                        let (used, window) = (message.usage.context(), cfg.watch.autocompact);
                        if used as f64 >= cfg.watch.handoff_at * window as f64
                            && task.sessions.len() <= cfg.watch.max_handoffs as usize
                        {
                            fresh = Some(format!("context at {used} of {window} tokens"));
                            bail!("handing off");
                        }
                    }
                    Event::RateLimitEvent { rate_limit_info: l } => {
                        if !matches!(l.status.as_str(), "allowed" | "allowed_warning") {
                            // ponytail: no reset time means a 5-minute guess
                            rejected = Some(l.resets_at.unwrap_or(sched::now() + 300));
                        }
                    }
                    Event::Result(r) => {
                        // never lower it: a crashed run can report zero
                        let spent = task.usage.entry(r.session_id.clone()).or_default();
                        *spent = spent.max(r.total_cost_usd);
                        task.save(state)?;
                        outcome = Some(next(&r, rejected, retried));
                        if r.errors
                            .iter()
                            .any(|e| e.starts_with("No conversation found"))
                        {
                            fresh = Some("the transcript is gone".to_string());
                        }
                        // a structured reply isn't a summary, so the last one stands
                        if schema.is_none() {
                            task.summary = Some(r.result).filter(|s| !s.is_empty());
                        }
                        reply = r.structured_output;
                        denied = r
                            .permission_denials
                            .iter()
                            .map(|d| call(&d.tool_name, &d.tool_input))
                            .collect();
                    }
                    _ => {}
                }
                Ok(())
            });
            if let Some(reason) = fresh {
                let line = serde_json::json!({"type": "handoff", "reason": reason});
                writeln!(log, "{}", redact(&line.to_string()))?;
                let said = said.or(task.summary.clone());
                prompt = handoff(task, &dir, base, said.as_deref(), instruction)?;
                resume = false;
                continue;
            }
            match outcome.unwrap_or(Next::Go) {
                Next::Go => {}
                Next::Fail(why) => bail!("{why}"),
                Next::Park(until) => {
                    sched::park(state, until)?;
                    std::thread::sleep(Duration::from_secs(until.saturating_sub(sched::now())));
                    prompt = "A rate limit paused you and has now reset. Carry on where you \
                              left off."
                        .into();
                    resume = true;
                    continue;
                }
                Next::Retry(status) => {
                    retried = true;
                    let step =
                        step.map_or(String::new(), |s| format!(" Your last step was `{s}`."));
                    prompt = format!(
                        "Your last turn ended on an API error ({status}).{step} Pick up from there."
                    );
                    resume = true;
                    continue;
                }
            }
            if let Err(e) = &res
                && let Some(nudge) = e.downcast_ref::<Nudge>()
            {
                ensure!(
                    task.nudges < cfg.watch.nudges,
                    "{} after {} nudge(s)",
                    nudge.reason,
                    task.nudges
                );
                task.nudges += 1;
                task.save(state)?;
                let line = serde_json::json!({"type": "nudge", "reason": nudge.reason});
                writeln!(log, "{}", redact(&line.to_string()))?;
                (prompt, resume) = (nudge.prompt.clone(), true);
                continue;
            }
            res?;
            // the worker may have found another way, so the gate decides; Review shows what was
            // denied, usually something allowed_tools is missing
            if !denied.is_empty() {
                let note = redact(&format!("Permission denied:\n- {}", denied.join("\n- ")));
                let summary = task
                    .summary
                    .take()
                    .map_or(note.clone(), |s| format!("{s}\n\n{note}"));
                task.summary = Some(summary);
                task.save(state)?;
            }
            return Ok(reply);
        }
    };

    // an optional Claude run, then the gate and, with `review`, the critic. A failing gate or an
    // open blocker or major goes back to the worker, for up to max_rounds fix rounds between
    // them; true if the gate passed
    let gate_log = logs.join(format!("{}.gate.log", task.id));
    let mut work = |task: &mut Task, first: Option<(&str, bool)>, review: bool| -> Result<bool> {
        if let Some((prompt, resume)) = first {
            session(task, prompt, resume, None)?;
        }
        let mut findings = Findings::load(state, &task.id).unwrap_or_default();
        let mut round = 0;
        loop {
            task.status = Status::Checking;
            task.save(state)?;
            let (checks, failures) = gate::run(&dir, base, &cfg.gate.steps, &env, &gate_log)?;
            task.gate = Some(checks);
            let last = round == cfg.critic.max_rounds;
            if !failures.is_empty() {
                if last {
                    return Ok(false);
                }
                let fix =
                    format!("The gate failed. Fix the failures below, then commit.\n\n{failures}");
                session(task, &fix, true, None)?;
                round += 1;
                continue;
            }
            if !review {
                return Ok(true);
            }
            // a critic that can't finish leaves a note, not a failed task
            findings =
                critic::run(task, &dir, base, &cfg, &env, &logs, &findings).unwrap_or_else(|e| {
                    Findings {
                        error: Some(format!("{e:#}")),
                        ..findings
                    }
                });
            findings.save(state, &task.id)?;
            let open = findings.open();
            if open.is_empty() || last || findings.error.is_some() {
                return Ok(true);
            }
            let reply = session(task, &critic::fix_prompt(&open), true, Some(critic::REPLY))?;
            findings.resolve(open, reply);
            findings.save(state, &task.id)?;
            round += 1;
        }
    };

    match (pr, reply) {
        (None, None) => {
            let prompt = prompt(task);
            work(task, Some((&prompt, false)), true)?;
        }
        (None, Some(reply)) => _ = work(task, Some((reply, true)), true)?,
        (Some(instruction), _) => {
            let passed = match rebase(&dir, base)? {
                None => work(task, Some((&conflict(base), true)), false)?,
                Some(true) => work(task, None, false)?,
                Some(false) => task.gate.iter().flatten().all(|c| c.passed),
            };
            if passed {
                // saved first, so a failed draft leaves the task in Review (see `run`)
                task.status = Status::Review;
                task.save(state)?;
                let instruction = Some(instruction).filter(|i| !i.is_empty());
                let findings = Findings::load(state, &task.id)?;
                let draft = pr::draft(task, &dir, base, &cfg.pr, instruction, &findings)?;
                task.pr_draft = Some(draft);
            }
        }
    }
    task.status = Status::Review;
    task.save(state)
}

fn conflict(base: &str) -> String {
    format!(
        "Rebasing this branch onto {base} conflicted, so yogan aborted it. Rebase onto {base} \
         and resolve the conflicts, keeping this task's intent, then make sure it still builds \
         and its tests pass."
    )
}

/// Fetches and rebases the slot's branch onto `base`. `None` means it conflicted and was
/// aborted; otherwise whether the head moved.
fn rebase(dir: &Path, base: &str) -> Result<Option<bool>> {
    git(dir, &["fetch", "--quiet", "origin"])?;
    let before = git(dir, &["rev-parse", "HEAD"])?;
    if git(dir, &["rebase", "--quiet", base]).is_err() {
        git(dir, &["rebase", "--abort"])?;
        return Ok(None);
    }
    Ok(Some(git(dir, &["rev-parse", "HEAD"])? != before))
}

/// Server names in an `.mcp.json`-style file.
fn mcp_servers(file: &Path) -> Result<Vec<String>> {
    let text = fs::read_to_string(file).with_context(|| file.display().to_string())?;
    let json: serde_json::Value = serde_json::from_str(&text)?;
    let servers = json["mcpServers"].as_object().into_iter().flatten();
    Ok(servers.map(|(name, _)| name.clone()).collect())
}

/// A run's `--max-budget-usd`: what's left of the task's `ceiling`, since the flag counts only
/// the run's own spend; none when the ceiling is 0, which turns it off.
fn budget_left(ceiling: f64, spent: f64) -> Result<Option<f64>> {
    if ceiling <= 0.0 {
        return Ok(None);
    }
    let left = ceiling - spent;
    ensure!(left > 0.0, "hit its spend ceiling of ${ceiling}");
    Ok(Some(left))
}

/// What a run's result calls for.
#[derive(Debug, PartialEq)]
enum Next {
    /// Carry on as usual.
    Go,
    /// A rate limit stopped it: wait until this Unix time, then resume.
    Park(u64),
    /// An API error ended it: resume once.
    Retry(u16),
    Fail(String),
}

/// `rejected` is the reset time of a rate limit the run reported as not allowed; `retried`
/// whether this session already resumed after an API error.
fn next(r: &RunResult, rejected: Option<u64>, retried: bool) -> Next {
    if r.subtype == "error_max_budget_usd" {
        return Next::Fail("hit its spend ceiling".into());
    }
    if !r.is_error {
        return Next::Go;
    }
    if let Some(until) = rejected {
        return Next::Park(until);
    }
    match r.api_error_status {
        Some(status) if retried => Next::Fail(format!("API error {status} again after a retry")),
        Some(status) => Next::Retry(status),
        None => Next::Go,
    }
}

/// A fresh session's prompt: the task, where the last session left off (`said`), the diff so
/// far, and the instruction it was on if that wasn't the task itself.
fn handoff(
    task: &Task,
    dir: &Path,
    base: &str,
    said: Option<&str>,
    instruction: &str,
) -> Result<String> {
    let mut p = prompt(task);
    p.push_str(
        "\nAn earlier session worked on this task in this worktree and can't go on; its commits \
         are on the branch and its uncommitted changes are in the worktree. Carry on from where \
         it left off.\n",
    );
    if let Some(said) = said {
        p.push_str(&format!("\nIts last words:\n{said}\n"));
    }
    let fork = git(dir, &["merge-base", base, "HEAD"])?;
    let diff = git(dir, &["diff", &fork])?;
    // ponytail: a cap so a runaway diff can't fill the new context; it can read the rest
    let cut: String = diff.chars().take(100_000).collect();
    let more = match cut.len() < diff.len() {
        true => "\n(cut short; run the git diff for the rest)",
        false => "",
    };
    p.push_str(&format!(
        "\nThe change so far, `git diff {fork}`:\n{cut}{more}\n"
    ));
    if instruction != prompt(task) {
        p.push_str(&format!(
            "\nIt was working on this instruction:\n{instruction}\n"
        ));
    }
    Ok(p)
}

fn prompt(task: &Task) -> String {
    let mut p = format!("{}\n\n{}\n", task.title, task.body);
    // the lead files 1-5; a task typed in compose has none
    if !task.acceptance.is_empty() {
        let criteria = task.acceptance.join("\n- ");
        p.push_str(&format!("\nAcceptance criteria:\n- {criteria}\n"));
    }
    if !task.crates.is_empty() {
        p.push_str(&format!("\nCrates: {}\n", task.crates.join(", ")));
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::process::getsid;
    use std::time::{Duration, Instant};

    fn result(json: &str) -> RunResult {
        let base = serde_json::json!({
            "subtype": "success", "is_error": false, "total_cost_usd": 0.5, "usage": {},
            "permission_denials": []
        });
        let mut v = base.as_object().unwrap().clone();
        v.extend(serde_json::from_str::<serde_json::Map<_, _>>(json).unwrap());
        serde_json::from_value(v.into()).unwrap()
    }

    #[test]
    fn budget_is_what_the_ceiling_leaves_or_off() {
        assert_eq!(budget_left(5.0, 1.5).unwrap(), Some(3.5));
        assert!(budget_left(5.0, 5.0).is_err());
        assert_eq!(budget_left(0.0, 99.0).unwrap(), None);
    }

    #[test]
    fn results_lead_to_go_park_retry_or_fail() {
        let ok = result("{}");
        assert_eq!(next(&ok, None, false), Next::Go);
        // a rate limit that let the run finish isn't a reason to park
        assert_eq!(next(&ok, Some(9), false), Next::Go);

        let budget = result(r#"{"subtype": "error_max_budget_usd", "is_error": true}"#);
        assert!(matches!(next(&budget, None, false), Next::Fail(_)));

        let api = result(r#"{"is_error": true, "api_error_status": 529}"#);
        assert_eq!(next(&api, None, false), Next::Retry(529));
        assert!(matches!(next(&api, None, true), Next::Fail(w) if w.contains("529")));
        // a rejected rate limit parks until its reset, retried or not
        let limited = result(r#"{"is_error": true, "api_error_status": 429}"#);
        assert_eq!(
            next(&limited, Some(1791519000), true),
            Next::Park(1791519000)
        );

        // any other error is left to the exit status
        assert_eq!(
            next(&result(r#"{"is_error": true}"#), None, false),
            Next::Go
        );
    }

    #[test]
    fn loop_detector_counts_identical_calls_in_a_row() {
        let mut r = Repeats::default();
        let test = serde_json::json!({"command": "cargo test -p a"});
        assert_eq!(r.see("Bash", &test), 1);
        assert_eq!(r.see("Bash", &test), 2);
        // a different input or tool breaks the run
        assert_eq!(r.see("Bash", &serde_json::json!({"command": "ls"})), 1);
        assert_eq!(r.see("Bash", &test), 1);
        assert_eq!(r.see("Read", &test), 1);
        assert_eq!(r.see("Read", &test), 2);
    }

    #[test]
    fn stall_detector_needs_silence_and_an_idle_tree() {
        let t0 = Instant::now();
        let at = |s| t0 + Duration::from_secs(s);
        let mut idle = Idle::new(t0);
        assert_eq!(idle.tick(at(10), 5_000, false), Duration::ZERO); // first sample: busy
        assert_eq!(idle.tick(at(20), 5_050, false), Duration::from_secs(10));
        // a build burning CPU without a stream event isn't a stall
        assert_eq!(idle.tick(at(30), 9_000, false), Duration::ZERO);
        assert_eq!(idle.tick(at(40), 9_000, false), Duration::from_secs(10));
        // nor is a build queued for a permit
        assert_eq!(idle.tick(at(50), 9_000, true), Duration::ZERO);
        // a child exiting drops the total, which isn't work
        assert_eq!(idle.tick(at(60), 1_000, false), Duration::from_secs(10));
        idle.event(at(65));
        assert_eq!(idle.tick(at(70), 1_000, false), Duration::from_secs(5));
    }

    #[test]
    fn a_held_queue_counts_as_queued() {
        let queue = std::env::temp_dir().join(format!("yogan-queue-{}", std::process::id()));
        assert!(!queued(&queue)); // no file: no build ever queued
        let f = File::create(&queue).unwrap();
        assert!(!queued(&queue));
        f.lock_shared().unwrap();
        assert!(queued(&queue));
        drop(f);
        // a test forking meanwhile holds the fd until it execs
        let start = Instant::now();
        while queued(&queue) {
            assert!(start.elapsed() < Duration::from_secs(5), "still queued");
            std::thread::sleep(Duration::from_millis(20));
        }
        fs::remove_file(&queue).unwrap();
    }

    #[test]
    fn a_loop_kills_the_run_and_names_the_call() {
        let log = std::env::temp_dir().join(format!("yogan-loop-{}", std::process::id()));
        let call = r#"{"type":"assistant","message":{"id":"m","usage":{},"content":[{"type":"tool_use","id":"t","name":"Bash","input":{"command":"cargo test"}}]}}"#;
        let mut cmd = Command::new("sh");
        // a child that outlives the stream, as a test run would
        let script = format!(
            "sleep 30 & echo $! > {0}.pid; for i in 1 2 3; do echo '{call}'; done; wait",
            log.display()
        );
        cmd.args(["-c", &script, "sh"]);
        let watch = config::Watch {
            stall_after: Duration::from_secs(3600),
            nudges: 1,
            loop_repeats: 3,
            autocompact: 200_000,
            handoff_at: 0.8,
            max_handoffs: 2,
        };
        let (mut out, err) = (
            File::create(&log).unwrap(),
            File::create("/dev/null").unwrap(),
        );
        let start = Instant::now();
        let e = claude(&mut cmd, "go", &mut out, &err, Some((&watch, &log)), |_| {
            Ok(())
        })
        .unwrap_err();
        let nudge = e.downcast_ref::<Nudge>().expect("a nudge");
        assert_eq!(nudge.reason, "repeated `Bash cargo test` 3 times");
        assert!(nudge.prompt.contains("hypothesis"), "{}", nudge.prompt);
        assert!(start.elapsed() < Duration::from_secs(10));
        let sleep: i32 = fs::read_to_string(log.with_extension("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            test_kill_process(Pid::from_raw(sleep).unwrap()).is_err(),
            "child survived"
        );
        let _ = fs::remove_file(log.with_extension("pid"));
        fs::remove_file(&log).unwrap();
    }

    #[test]
    fn detached_in_own_session_and_stopped_as_a_group() {
        let dir = std::env::temp_dir().join(format!("yogan-detach-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let grandchild = dir.join("pid");
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(format!(
            "sleep 30 & echo $! > {}; wait",
            grandchild.display()
        ));
        let pid = detach(cmd).unwrap();

        // its own session: a hangup for the spawning terminal's session never reaches it
        let p = Pid::from_raw(pid as i32).unwrap();
        assert_eq!(getsid(Some(p)).unwrap(), p);
        assert_ne!(getsid(None).unwrap(), p);

        let start = Instant::now();
        while !grandchild.exists() || fs::read_to_string(&grandchild).unwrap().is_empty() {
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(20));
        }
        let sleep: i32 = fs::read_to_string(&grandchild)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let sleep = Pid::from_raw(sleep).unwrap();

        // stop reaches the grandchild too, as it would a cargo build under Claude
        stop(pid).unwrap();
        let start = Instant::now();
        while test_kill_process(sleep).is_ok() {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "grandchild survived"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        fs::remove_dir_all(&dir).unwrap();
    }
}
