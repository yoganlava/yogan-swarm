use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use yogan_swarm::task::{self, Status, Task};

/// Stand-in for Claude: an init event, a leaked token, and where it ran.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
echo '{"type":"system","subtype":"init","session_id":"s-1","model":"m","tools":[]}'
echo '{"type":"assistant","text":"ghp_aB3aB3aB3aB3aB3aB3aB3aB3"}'
pwd > "$YOGAN_ROOT/../claude.cwd"
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

    let project = root.join("project.toml");
    let global = format!("[projects]\norigin = \"{}\"\n", project.display());
    fs::write(home.join(".config/yogan/config.toml"), global).unwrap();
    let claude = bin.join("claude");
    fs::write(&claude, FAKE_CLAUDE).unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());

    let worker = |id: &str, setup: &str| {
        fs::write(&project, format!("[scripts]\nsetup = \"{setup}\"\n")).unwrap();
        let task = Task {
            id: id.into(),
            title: "Do it".into(),
            branch: format!("u/{id}"),
            status: Status::Approved,
            ..Default::default()
        };
        task.save(&state).unwrap();
        let ok = Command::new(env!("CARGO_BIN_EXE_yogan"))
            .args(["worker", id])
            .current_dir(&repo)
            .env("HOME", &home)
            .env("YOGAN_DIR", &state)
            .env("PATH", &path)
            .status()
            .unwrap()
            .success();
        let tasks = task::load_all(&state).unwrap();
        (ok, tasks.into_iter().find(|t| t.id == id).unwrap())
    };

    let (ok, t1) = worker("t1", "echo ready");
    assert!(ok);
    assert_eq!(t1.status, Status::Review);
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
    let (ok, t2) = worker("t2", "echo db down; exit 3");
    assert!(!ok);
    assert_eq!((t2.status, t2.slot), (Status::Failed, Some(2)));
    let summary = t2.summary.unwrap();
    assert!(summary.contains("logs/t2.setup.log"), "{summary}");
    assert!(!state.join("logs/t2.jsonl").exists());

    fs::remove_dir_all(&root).unwrap();
}
