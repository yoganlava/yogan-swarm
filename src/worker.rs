//! `yogan worker <id>`: one task's whole lifecycle, run detached in its own session.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, ensure};
use rustix::process::{Pid, Signal, kill_process_group, setsid};

use crate::redact::redact;
use crate::stream::{Event, System};
use crate::task::{self, Status, Task};
use crate::{config, slot};

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

/// The worker's main. Any error marks the task `Failed`, with the error as its summary.
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
    res
}

fn lifecycle(repo: &Path, state: &Path, task: &mut Task) -> Result<()> {
    let cfg = config::load(repo)?;
    let tasks = task::load_all(state)?;
    let (n, _lock) = slot::claim(state, &tasks, cfg.worker.slots)?.context("no free slot")?;
    task.status = Status::Running;
    task.slot = Some(n);
    task.pid = Some(std::process::id());
    task.save(state)?;

    // ponytail: always bases on origin/main; T23 bases on the parent's branch
    let dir = slot::prepare(repo, state, n, &task.branch, "origin/main")?;
    let env = slot::env(repo, &dir, n, task, cfg.ports.as_ref());
    let logs = state.join("logs");
    let script = cfg.scripts.as_ref().and_then(|s| s.setup.as_deref());
    let setup_log = logs.join(format!("{}.setup.log", task.id));
    slot::setup(repo, &dir, &env, script, &setup_log)?;

    // the lead always files 1-5 acceptance criteria
    let prompt = format!(
        "{}\n\n{}\n\nAcceptance criteria:\n- {}\n",
        task.title,
        task.body,
        task.acceptance.join("\n- ")
    );
    let mut claude = Command::new("claude")
        .args(["-p", &prompt, "--output-format", "stream-json", "--verbose"])
        .current_dir(&dir)
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .context("starting claude")?;
    fs::create_dir_all(&logs)?;
    let mut log = File::create(logs.join(format!("{}.jsonl", task.id)))?;
    let stdout = claude.stdout.take().context("claude stdout")?;
    for line in BufReader::new(stdout).lines() {
        let line = line?;
        writeln!(log, "{}", redact(&line))?;
        if let Ok(Event::System(System::Init { session_id, .. })) = serde_json::from_str(&line) {
            task.sessions.push(session_id);
            task.save(state)?;
        }
    }
    let status = claude.wait()?;
    ensure!(status.success(), "claude exited with {status}");
    // ponytail: straight to Review until the gate (T12) adds Checking
    task.status = Status::Review;
    task.save(state)
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
