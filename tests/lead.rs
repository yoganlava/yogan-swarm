use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use yogan_swarm::lead::{self, Phase, Request};
use yogan_swarm::task::{self, Status};

/// Stand-in for Claude: records its args, then files one task through `yogan` on its PATH.
/// A request of `fail` exits 1.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
printf '%s\n' "$@" > "$YOGAN_DIR/claude.args"
[ "$2" = fail ] && exit 1
echo '{"type":"system","subtype":"init","session_id":"s-lead","model":"m","tools":[],"mcp_servers":[]}'
id=$(yogan task propose --title "Reject negative max_delay" --crates ledger \
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

    let lead = |id: &str, text: &str| {
        let req = Request {
            id: id.into(),
            text: text.into(),
            ticket: Some("CC-687".into()),
            ..Default::default()
        };
        req.save(&state).unwrap();
        let ok = Command::new(env!("CARGO_BIN_EXE_yogan"))
            .args(["lead", id])
            .current_dir(&repo)
            .env("HOME", &home)
            .env("YOGAN_DIR", &state)
            .env("PATH", &path)
            .status()
            .unwrap()
            .success();
        (ok, lead::load(&state, id).unwrap())
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

    let (ok, r2) = lead("r2", "fail");
    assert!(!ok);
    assert_eq!(r2.status, Phase::Failed);
    assert!(r2.summary.unwrap().contains("claude exited"));

    fs::remove_dir_all(&root).unwrap();
}
