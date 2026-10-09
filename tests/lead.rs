use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use yogan_swarm::lead::{self, Mode, Phase, Request};
use yogan_swarm::task::{self, Status};

/// Stand-in for Claude: records its args, then files one task through `yogan` on its PATH, a
/// revised one when resumed. A request of `fail` exits 1, and `die` exits 1 after starting its
/// session; `flaky` fails once. Asked a question (it has WebSearch), it answers and files
/// nothing. Like claude, it rejects a prompt that isn't last after `--`.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
printf '%s\n' "$@" > "$YOGAN_DIR/claude.args"
for prompt; do :; done
eval "sep=\${$(($# - 1))}"
[ "$sep" = "--" ] || { echo "error: unknown option '$prompt'" >&2; exit 1; }
[ "$prompt" = fail ] && exit 1
echo '{"type":"system","subtype":"init","session_id":"s-lead","model":"m","tools":[],"mcp_servers":[]}'
[ "$prompt" = die ] && exit 1
if [ "$prompt" = flaky ] && [ ! -e "$YOGAN_DIR/flaky.done" ]; then
  touch "$YOGAN_DIR/flaky.done"; exit 1
fi
answer="See src/lib.rs:1 for it."
case "$*" in *--resume*) answer="Because." ;; esac
case "$*" in *WebSearch*)
  echo '{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.1,"usage":{"input_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1},"permission_denials":[],"result":"'"$answer"'"}'
  exit 0 ;; esac
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
    let lead = |id: &str, text: &str, mode| {
        let req = Request {
            id: id.into(),
            text: text.into(),
            ticket: Some("CC-687".into()),
            mode,
            ..Default::default()
        };
        req.save(&state).unwrap();
        yogan(&[id])
    };

    let (ok, r1) = lead("r1", "Reject a negative max_delay", Mode::Auto);
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
    assert!(
        !lead::answer_path(&state, "r1").exists(),
        "it planned, so no answer"
    );

    let args = fs::read_to_string(state.join("claude.args")).unwrap();
    for want in [
        "--\nReject a negative max_delay\n",
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

    let (ok, r2) = lead("r2", "fail", Mode::Plan);
    assert!(!ok);
    assert_eq!(r2.status, Phase::Failed);
    assert!(r2.summary.unwrap().contains("claude exited"));

    // t: a lead that never got a session plans afresh
    assert_eq!(lead::retry(&state, "r2").unwrap(), None);
    assert_eq!(lead::load(&state, "r2").unwrap().status, Phase::Planning);
    let again = lead::retry(&state, "r2").unwrap_err();
    assert_eq!(again.to_string(), "only a failed request can be retried");

    // one that died mid-session resumes it, told why it stopped, and files its proposals
    let (ok, r3) = lead("r3", "die", Mode::Plan);
    assert!(!ok);
    assert_eq!(
        (r3.status, r3.session.as_deref()),
        (Phase::Failed, Some("s-lead"))
    );
    let prompt = lead::retry(&state, "r3").unwrap().unwrap();
    assert!(
        prompt.contains("stopped before finishing: claude exited"),
        "{prompt}"
    );
    let (ok, r3) = yogan(&["r3", "--reply", &prompt]);
    assert!(ok);
    assert_eq!(r3.status, Phase::Done);
    let filed = task::load_all(&state).unwrap();
    assert!(
        filed
            .iter()
            .any(|t| t.plan == "r3" && t.status == Status::Proposed)
    );

    // Ask: the [ask] model researches, can't run Bash or file tasks, and its answer is kept
    let (ok, r4) = lead("r4", "How are tasks saved?", Mode::Ask);
    assert!(ok);
    assert_eq!(r4.status, Phase::Done);
    let answer = || fs::read_to_string(lead::answer_path(&state, "r4")).unwrap();
    assert_eq!(answer(), "See src/lib.rs:1 for it.\n");
    let tasks = task::load_all(&state).unwrap();
    assert!(
        !tasks.iter().any(|t| t.plan == "r4"),
        "a question files no tasks"
    );
    let args = fs::read_to_string(state.join("claude.args")).unwrap();
    for want in [
        "--model\nclaude-opus-5-5\n--effort\nhigh\n",
        "--allowedTools\nRead\nGrep\nGlob\nWebSearch\nWebFetch\n",
        "--disallowedTools\nEdit\nWrite\nNotebookEdit\nBash\n",
    ] {
        assert!(args.contains(want), "{want:?} not in {args}");
    }
    assert!(!args.contains("yogan task propose"), "{args}");

    // r: a follow-up resumes the session, and its answer goes under the question
    lead::follow_up(&repo, &state, "r4", "- and why?").unwrap();
    let (ok, r4) = yogan(&["r4", "--reply", "- and why?"]);
    assert!(ok);
    assert_eq!(r4.status, Phase::Done);
    assert_eq!(
        answer(),
        "See src/lib.rs:1 for it.\n\n---\n\n**- and why?**\n\nBecause.\n"
    );

    // t after a failed follow-up asks that follow-up again, keeping the answers so far
    lead::follow_up(&repo, &state, "r4", "flaky").unwrap();
    let (ok, r4) = yogan(&["r4", "--reply", "flaky"]);
    assert!(!ok);
    assert_eq!(r4.status, Phase::Failed);
    assert_eq!(lead::retry(&state, "r4").unwrap().as_deref(), Some("flaky"));
    let (ok, r4) = yogan(&["r4", "--reply", "flaky"]);
    assert!(ok);
    assert_eq!((r4.status, r4.follow_up), (Phase::Done, None));
    assert!(
        answer().ends_with("**- and why?**\n\nBecause.\n\n---\n\n**flaky**\n\nBecause.\n"),
        "{}",
        answer()
    );

    fs::remove_dir_all(&root).unwrap();
}
