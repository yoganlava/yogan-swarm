use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use rustix::process::{Pid, test_kill_process};

use yogan_swarm::task::{self, Status, Task};

/// Stand-in for Claude: records its args, cwd and effort env, then emits an init with
/// `$MCP` as its server, a leaked token, and a result denying `$DENIED`. Resumed, it commits.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
out="$YOGAN_ROOT/.."
printf '%s\n' "$@" > "$out/claude.args"
pwd > "$out/claude.cwd"
echo "${CLAUDE_CODE_EFFORT_LEVEL-unset}" > "$out/claude.effort"
ulimit -n > "$out/claude.nofile"
echo 'warning: token ghp_aB3aB3aB3aB3aB3aB3aB3aB3 expired' >&2
echo '{"type":"system","subtype":"init","session_id":"s-1","model":"m","tools":[],"mcp_servers":[{"name":"'"$MCP"'"}]}'
echo '{"type":"assistant","text":"ghp_aB3aB3aB3aB3aB3aB3aB3aB3"}'
echo '{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.1,"usage":{},"permission_denials":['"$DENIED"']}'
# only a gate fix round commits, so the first gate fails on "new commits"
case "$*" in *--resume*) git -c user.name=t -c user.email=t@t -c commit.gpgsign=false \
  commit -q --allow-empty -m fix ;; esac
"#;

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

#[test]
fn worker_runs_setup_then_claude() {
    let root = std::env::temp_dir().join(format!("yogan-worker-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let (repo, home, bin, state) = (
        root.join("repo"),
        root.join("home"),
        root.join("bin"),
        root.join("state"),
    );
    for dir in [&repo, &home.join(".config/yogan"), &bin] {
        fs::create_dir_all(dir).unwrap();
    }
    let origin = root.join("origin.git");
    git(
        &root,
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    );
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("a.txt"), "a").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "init"]);
    git(
        &repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&repo, &["push", "-q", "origin", "main"]);
    // untracked, so setup copies it into the slot
    fs::write(repo.join(".mcp.json"), r#"{"mcpServers":{"graft":{}}}"#).unwrap();

    let project = root.join("project.toml");
    let global = format!("[projects]\norigin = \"{}\"\n", project.display());
    fs::write(home.join(".config/yogan/config.toml"), global).unwrap();
    let claude = bin.join("claude");
    fs::write(&claude, FAKE_CLAUDE).unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());

    let worker = |id: &str, setup: &str, mcp: &str, denied: &str, effort: Option<&str>| {
        let worker_cfg = "[worker]\nmcp_config = \".mcp.json\"\nallowed_tools = [\"Read\"]\n\
                          read_tools = [\"mcp__graft__find\"]\ndeny = [\"Bash(curl *)\"]\n\
                          slots = 9\nconcurrency = 1\n";
        let scripts = format!("[scripts]\nsetup = \"{setup}\"\n[cargo]\nnofile = 777\n");
        fs::write(&project, format!("{worker_cfg}{scripts}")).unwrap();
        let task = Task {
            id: id.into(),
            title: "Do it".into(),
            branch: format!("u/{id}"),
            status: Status::Approved,
            model: Some("opus-x".into()),
            effort: effort.map(Into::into),
            ..Default::default()
        };
        task.save(&state).unwrap();
        let ok = Command::new(env!("CARGO_BIN_EXE_yogan"))
            .args(["worker", id])
            .current_dir(&repo)
            .env("HOME", &home)
            .env("YOGAN_DIR", &state)
            .env("PATH", &path)
            .env("MCP", mcp)
            .env("DENIED", denied)
            .env("CLAUDE_CODE_EFFORT_LEVEL", "low")
            .status()
            .unwrap()
            .success();
        let tasks = task::load_all(&state).unwrap();
        (ok, tasks.into_iter().find(|t| t.id == id).unwrap())
    };

    let (ok, t1) = worker("t1", "echo ready", "graft", "", None);
    assert!(ok);
    assert_eq!(t1.status, Status::Review);
    // the first gate found no commits; the fix round resumed the same session and committed
    let gate = t1.gate.as_ref().unwrap();
    assert!(gate.iter().all(|c| c.passed), "{gate:?}");
    let gate_log = fs::read_to_string(state.join("logs/t1.gate.log")).unwrap();
    assert!(gate_log.contains("== new commits: ok"), "{gate_log}");
    // the task's model beats config; effort falls back to the [worker] default
    assert_eq!(t1.model.as_deref(), Some("opus-x"));
    assert_eq!(t1.effort.as_deref(), Some("high"));
    let args = fs::read_to_string(root.join("claude.args")).unwrap();
    for want in [
        "--model\nopus-x\n--effort\nhigh\n",
        "--setting-sources\nproject\n",
        "--strict-mcp-config\n",
        "--mcp-config\n.mcp.json\n--resume\ns-1\n",
        "--allowedTools\nRead\nmcp__graft__find\n--disallowedTools\nBash(git push *)\n",
        "WebFetch\nBash(curl *)\n--append-system-prompt\n",
    ] {
        assert!(args.contains(want), "{want:?} not in {args}");
    }
    let effort_env = fs::read_to_string(root.join("claude.effort")).unwrap();
    assert_eq!(effort_env, "unset\n");
    let nofile = fs::read_to_string(root.join("claude.nofile")).unwrap();
    assert_eq!(nofile, "777\n");
    assert_eq!(
        fs::read_to_string(state.join("logs/t1.stderr.log")).unwrap(),
        "warning: token [REDACTED] expired\n".repeat(2) // first run and the fix round
    );
    assert_eq!((t1.slot, t1.sessions), (Some(1), vec!["s-1".to_string()]));
    assert!(t1.pid.is_some());
    let slot = state.join("slots/1");
    let cwd = fs::read_to_string(root.join("claude.cwd")).unwrap();
    let cwd = Path::new(cwd.trim()).canonicalize().unwrap();
    assert_eq!(cwd, slot.canonicalize().unwrap());
    assert_eq!(
        fs::read_to_string(state.join("logs/t1.setup.log")).unwrap(),
        "ready\n"
    );
    let log = fs::read_to_string(state.join("logs/t1.jsonl")).unwrap();
    assert!(log.contains("\"session_id\":\"s-1\""), "{log}");
    assert!(log.contains("[REDACTED]") && !log.contains("ghp_"), "{log}");

    // a failing setup fails the task with its log path, before Claude starts
    let (ok, t2) = worker("t2", "echo db down; exit 3", "graft", "", None);
    assert!(!ok);
    assert_eq!((t2.status, t2.slot), (Status::Failed, Some(2)));
    let summary = t2.summary.unwrap();
    assert!(summary.contains("logs/t2.setup.log"), "{summary}");
    assert!(!state.join("logs/t2.jsonl").exists());

    // an MCP server the project didn't configure stops the worker
    let (ok, t3) = worker("t3", "true", "notion", "", None);
    assert!(!ok);
    assert_eq!(t3.status, Status::Failed);
    assert_eq!(
        t3.summary.as_deref(),
        Some("unexpected MCP servers: notion")
    );

    // a denied tool means the work is incomplete
    let denial = r#"{"tool_name":"Write","tool_input":{}}"#;
    let (ok, t4) = worker("t4", "true", "graft", denial, None);
    assert!(!ok);
    assert_eq!(t4.status, Status::Failed);
    let summary = t4.summary.unwrap();
    assert_eq!(summary, "incomplete: permission denied for Write");

    // an unknown effort fails before Claude starts
    let (ok, t5) = worker("t5", "true", "graft", "", Some("bogus"));
    assert!(!ok);
    assert_eq!(t5.status, Status::Failed);
    assert!(t5.summary.unwrap().contains("\"bogus\""));
    assert!(!state.join("logs/t5.jsonl").exists());

    // queued tasks drain one at a time (concurrency 1), each worker starting the next on exit
    for id in ["t6", "t7"] {
        let task = Task {
            id: id.into(),
            branch: format!("u/{id}"),
            status: Status::Approved,
            ..Default::default()
        };
        task.save(&state).unwrap();
    }
    let (ok, _) = worker("t8", "true", "graft", "", None);
    assert!(ok);
    let start = Instant::now();
    loop {
        let tasks = task::load_all(&state).unwrap();
        // done once both reached Review and their workers have exited
        let done = |id: &str| {
            let t = tasks.iter().find(|t| t.id == id).unwrap();
            let pid = Pid::from_raw(t.pid.unwrap() as i32).unwrap();
            t.status == Status::Review && test_kill_process(pid).is_err()
        };
        if done("t6") && done("t7") {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "queue stuck: {tasks:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    fs::remove_dir_all(&root).unwrap();
}
