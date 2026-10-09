use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use yogan_swarm::lead::{self, Phase, Request};
use yogan_swarm::task::{self, Status};

/// Stand-in for Claude: records its args, then files one task through `yogan` on its PATH, a
/// revised one when resumed. A request of `fail` exits 1.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
printf '%s\n' "$@" > "$YOGAN_DIR/claude.args"
[ "$2" = fail ] && exit 1
echo '{"type":"system","subtype":"init","session_id":"s-lead","model":"m","tools":[],"mcp_servers":[]}'
title="Reject negative max_delay"
case "$*" in *--resume*) title="Reject negative max_delay and min_delay" ;; esac
id=$(yogan task propose --title "$title" --crates ledger \
  --accept "a negative max_delay is rejected at parse time" "It panics later.") || exit 1
echo '{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.1,"usage":{"input_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1},"permission_denials":[],"result":"filed '"$id"'"}'
"#;

#[test]
fn lead_files_proposals() {
    let root = std::env::temp_dir().join(format!("yogan-lead-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let (repo, home, bin, state) = (
        root.join("repo"),
        root.join("home"),
        root.join("bin"),
        root.join("state"),
    );
    for dir in [&repo, &home, &bin] {
        fs::create_dir_all(dir).unwrap();
    }
    let claude = bin.join("claude");
    fs::write(&claude, FAKE_CLAUDE).unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
    // yogan itself isn't on this PATH; the lead adds it
    let path = format!("{}:/usr/bin:/bin", bin.display());

    // runs `yogan lead <args>`; returns whether it succeeded and the request after
    let yogan = |args: &[&str]| {
        let ok = Command::new(env!("CARGO_BIN_EXE_yogan"))
            .arg("lead")
            .args(args)
            .current_dir(&repo)
            .env("HOME", &home)
            .env("YOGAN_DIR", &state)
            .env("PATH", &path)
            .status()
            .unwrap()
            .success();
        (ok, lead::load(&state, args[0]).unwrap())
    };
    let lead = |id: &str, text: &str| {
        let req = Request {
            id: id.into(),
            text: text.into(),
            ticket: Some("CC-687".into()),
            ..Default::default()
        };
        req.save(&state).unwrap();
        yogan(&[id])
    };

    let (ok, r1) = lead("r1", "Reject a negative max_delay");
    assert!(ok);
    assert_eq!(r1.status, Phase::Done);
    assert_eq!(r1.session.as_deref(), Some("s-lead"));
    let tasks = task::load_all(&state).unwrap();
    assert_eq!(tasks.len(), 1);
    let t = &tasks[0];
    assert_eq!(r1.summary, Some(format!("filed {}", t.id)));
    assert_eq!(t.status, Status::Proposed);
    assert_eq!(
        (t.plan.as_str(), t.ticket.as_deref()),
        ("r1", Some("CC-687"))
    );
    assert_eq!(t.crates, ["ledger"]);
    assert_eq!(t.branch, "reject-negative-max-delay");

    let args = fs::read_to_string(state.join("claude.args")).unwrap();
    for want in [
        "-p\nReject a negative max_delay\n",
        "--model\nclaude-opus-5-5\n--effort\nxhigh\n",
        "--allowedTools\nRead\nGrep\nGlob\nBash(yogan task propose:*)\n",
        "--disallowedTools\nEdit\nWrite\nNotebookEdit\n--append-system-prompt\n",
    ] {
        assert!(args.contains(want), "{want:?} not in {args}");
    }

    // r: the pending proposal is withdrawn and the resumed lead files the revised plan
    let prompt = lead::reply(&state, "r1", "- cover min_delay too").unwrap();
    assert!(prompt.starts_with("- cover min_delay too\n"), "{prompt}");
    assert!(prompt.contains(&format!("- {} “Reject negative max_delay”", t.id)));
    let again = lead::reply(&state, "r1", "and more").unwrap_err();
    assert_eq!(again.to_string(), "the lead is still planning");
    let (ok, r1) = yogan(&["r1", "--reply", &prompt]);
    assert!(ok);
    assert_eq!(r1.status, Phase::Done);
    let args = fs::read_to_string(state.join("claude.args")).unwrap();
    assert!(args.contains("--resume\ns-lead\n"), "{args}");
    let tasks = task::load_all(&state).unwrap();
    let status = |title: &str| tasks.iter().find(|t| t.title == title).unwrap().status;
    assert_eq!(status("Reject negative max_delay"), Status::Discarded);
    assert_eq!(
        status("Reject negative max_delay and min_delay"),
        Status::Proposed
    );

    let (ok, r2) = lead("r2", "fail");
    assert!(!ok);
    assert_eq!(r2.status, Phase::Failed);
    assert!(r2.summary.unwrap().contains("claude exited"));

    fs::remove_dir_all(&root).unwrap();
}
