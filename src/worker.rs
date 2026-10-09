//! `yogan worker <id>`: one task's whole lifecycle, run detached in its own session.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};
use rustix::process::{
    Pid, Resource, Rlimit, Signal, getrlimit, kill_process_group, setrlimit, setsid,
};

use crate::redact::redact;
use crate::stream::{Event, System};
use crate::task::{self, Status, Task};
use crate::{config, gate, shim, slot};

const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

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
- If YOGAN_PORT_BASE is set, your ports are YOGAN_PORT_BASE up to YOGAN_PORT_BASE + \
YOGAN_PORT_COUNT - 1; never bind any other.
- End with a plain summary that someone without context can follow, flagging anything you were \
unsure of.";

/// Starts `yogan worker <id>` for the checkout `repo` in a new session, so neither Ctrl-C
/// nor a closing terminal reaches it. Returns its pid, which is also its process group.
pub fn spawn(repo: &Path, id: &str) -> Result<u32> {
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.args(["worker", id]).current_dir(repo);
    detach(cmd)
}

fn detach(mut cmd: Command) -> Result<u32> {
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

/// The worker's main. Any error marks the task `Failed`, with the error as its summary. On exit it
/// starts whatever is ready next.
pub fn run(repo: &Path, id: &str) -> Result<()> {
    let state = task::state_dir(repo)?;
    let mut task = task::load_all(&state)?
        .into_iter()
        .find(|t| t.id == id)
        .with_context(|| format!("no task {id}"))?;
    let res = lifecycle(repo, &state, &mut task);
    if let Err(e) = &res {
        task.status = Status::Failed;
        task.summary = Some(format!("{e:#}"));
        task.save(&state)?;
    }
    // this worker's slot lock is released by now, so the next task can take it
    let next = crate::sched::run(repo);
    res?;
    next
}

fn lifecycle(repo: &Path, state: &Path, task: &mut Task) -> Result<()> {
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
    let (n, _lock) = slot::claim(state, &tasks, cfg.worker.slots)?.context("no free slot")?;
    let model = task.model.clone().unwrap_or(cfg.worker.model.clone());
    let effort = task.effort.clone().unwrap_or(cfg.worker.effort.clone());
    // claude only warns on an unknown effort and runs on its default
    ensure!(
        EFFORTS.contains(&effort.as_str()),
        "unknown effort {effort:?}, expected one of {}",
        EFFORTS.join(", ")
    );
    task.model = Some(model.clone());
    task.effort = Some(effort.clone());
    task.status = Status::Running;
    task.slot = Some(n);
    task.pid = Some(std::process::id());
    task.save(state)?;

    // ponytail: always bases on origin/main; T23 bases on the parent's branch
    let base = "origin/main";
    let dir = slot::prepare(repo, state, n, &task.branch, base)?;
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
    slot::setup(repo, &dir, &env, script, &setup_log)?;

    let w = &cfg.worker;
    let allowed = [&w.allowed_tools[..], &w.read_tools[..]].concat();
    ensure!(!allowed.is_empty(), "[worker] allowed_tools is empty");
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

    // one Claude run in the slot; `resume` continues the task's last session
    let mut session = |task: &mut Task, prompt: &str, resume: bool| -> Result<()> {
        let mut cmd = Command::new("claude");
        cmd.args(["-p", prompt, "--output-format", "stream-json", "--verbose"])
            .args([
                "--permission-mode",
                "acceptEdits",
                "--setting-sources",
                "project",
            ])
            .args([
                "--model",
                &model,
                "--effort",
                &effort,
                "--strict-mcp-config",
            ]);
        if let Some(file) = &w.mcp_config {
            cmd.args(["--mcp-config", file]);
        }
        if resume {
            cmd.args([
                "--resume",
                task.sessions.last().context("no session to resume")?,
            ]);
        }
        let mut claude = cmd
            .arg("--allowedTools")
            .args(&allowed)
            .arg("--disallowedTools")
            .args(DENY)
            .args(&w.deny)
            .args(["--append-system-prompt", RULES])
            .current_dir(&dir)
            .env_remove("CLAUDE_CODE_EFFORT_LEVEL") // it would override --effort
            .envs(env.iter().cloned())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("starting claude")?;
        let stderr = claude.stderr.take().context("claude stderr")?;
        let mut err_log = err_log.try_clone()?;
        let stderr = std::thread::spawn(move || -> std::io::Result<()> {
            for line in BufReader::new(stderr).lines() {
                writeln!(err_log, "{}", redact(&line?))?;
            }
            Ok(())
        });
        let stdout = claude.stdout.take().context("claude stdout")?;
        let mut denied = Vec::new();
        for line in BufReader::new(stdout).lines() {
            let line = line?;
            writeln!(log, "{}", redact(&line))?;
            match serde_json::from_str(&line) {
                Ok(Event::System(System::Init {
                    session_id,
                    mcp_servers,
                    ..
                })) => {
                    if !task.sessions.contains(&session_id) {
                        task.sessions.push(session_id);
                        task.save(state)?;
                    }
                    let extra: Vec<_> = mcp_servers
                        .into_iter()
                        .filter(|s| !servers.contains(&s.name))
                        .map(|s| s.name)
                        .collect();
                    if !extra.is_empty() {
                        claude.kill()?;
                        bail!("unexpected MCP servers: {}", extra.join(", "));
                    }
                }
                Ok(Event::Result(r)) => {
                    denied = r
                        .permission_denials
                        .into_iter()
                        .map(|d| d.tool_name)
                        .collect();
                }
                _ => {}
            }
        }
        let status = claude.wait()?;
        let _ = stderr.join();
        ensure!(status.success(), "claude exited with {status}");
        ensure!(
            denied.is_empty(),
            "incomplete: permission denied for {}",
            denied.join(", ")
        );
        Ok(())
    };

    session(task, &prompt(task), false)?;
    let gate_log = logs.join(format!("{}.gate.log", task.id));
    for round in 0.. {
        task.status = Status::Checking;
        task.save(state)?;
        let (checks, failures) = gate::run(&dir, base, &cfg.gate.steps, &env, &gate_log)?;
        task.gate = Some(checks);
        if failures.is_empty() || round == cfg.critic.max_rounds {
            break;
        }
        task.status = Status::Running;
        task.save(state)?;
        let fix = format!("The gate failed. Fix the failures below, then commit.\n\n{failures}");
        session(task, &fix, true)?;
    }
    task.status = Status::Review;
    task.save(state)
}

/// Server names in an `.mcp.json`-style file.
fn mcp_servers(file: &Path) -> Result<Vec<String>> {
    let text = fs::read_to_string(file).with_context(|| file.display().to_string())?;
    let json: serde_json::Value = serde_json::from_str(&text)?;
    let servers = json["mcpServers"].as_object().into_iter().flatten();
    Ok(servers.map(|(name, _)| name.clone()).collect())
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
    use rustix::process::{getsid, test_kill_process};
    use std::time::{Duration, Instant};

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
