use std::fs;
use std::path::Path;
use std::process::Command;

use yogan_swarm::critic::{self, Severity};
use yogan_swarm::{config, task::Task};

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

/// Runs the real `[critic]` on a seeded diff that claims to meet a criterion but doesn't:
/// `cargo test --test critic -- --ignored --nocapture`.
#[test]
#[ignore = "runs the real critic model"]
fn flags_an_unmet_criterion_as_a_blocker() {
    let repo = std::env::temp_dir().join(format!("yogan-critic-{}", std::process::id()));
    let _ = fs::remove_dir_all(&repo);
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"delay\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let parse = "pub fn parse_max_delay(s: &str) -> Result<i64, String> {\n    \
                 s.trim().parse().map_err(|e| format!(\"max_delay: {e}\"))\n}\n";
    fs::write(repo.join("src/lib.rs"), parse).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "init"]);
    git(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    // the "fix" only documents the rule; a negative value still parses
    fs::write(
        repo.join("src/lib.rs"),
        format!("/// Rejects a negative max_delay.\n{parse}"),
    )
    .unwrap();
    git(
        &repo,
        &["commit", "-qam", "fix(delay): reject negative max_delay"],
    );

    let task = Task {
        id: "t1".into(),
        title: "Reject negative max_delay".into(),
        body: "A negative max_delay panics later in the retry loop.".into(),
        crates: vec!["delay".into()],
        acceptance: vec!["parse_max_delay(\"-1\") returns an error".into()],
        ..Default::default()
    };
    let cfg = config::load(&repo).unwrap();
    let review = critic::run(&task, &repo, "origin/main", &cfg, &[], &repo.join("logs")).unwrap();
    println!("{review:#?}");
    assert!(
        review
            .findings
            .iter()
            .any(|f| f.severity == Severity::Blocker),
        "no proven blocker: {review:#?}"
    );
    fs::remove_dir_all(&repo).unwrap();
}
