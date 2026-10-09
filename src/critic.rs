//! The critic: a fresh, read-only session on another model that tries to break the change.

use std::fs::{self, File};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::stream::Event;
use crate::task::{self, Task};
use crate::worker;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Wrong behaviour, with reproducible evidence.
    Blocker,
    /// A likely bug, or a missing test for a stated requirement.
    Major,
    /// Style, naming, small improvements.
    Minor,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    /// `path:line`.
    pub location: String,
    pub claim: String,
    /// A failing test or command, or a call path from a real caller; empty when unproven.
    pub evidence: String,
}

/// `findings/<id>.toml`.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Findings {
    /// Proven blockers and majors, and every minor.
    pub findings: Vec<Finding>,
    /// Blockers and majors without evidence: shown in Review, never sent back.
    pub optional: Vec<Finding>,
    /// Why the critic didn't finish, if it didn't.
    pub error: Option<String>,
}

impl Findings {
    pub fn save(&self, state: &Path, id: &str) -> Result<()> {
        task::write_toml(&state.join("findings"), id, self)
    }
}

const SCHEMA: &str = r#"{"type":"object","properties":{"findings":{"type":"array","items":{"type":"object","properties":{"severity":{"enum":["blocker","major","minor"]},"location":{"type":"string","description":"path:line"},"claim":{"type":"string"},"evidence":{"type":"string"}},"required":["severity","location","claim","evidence"]}}},"required":["findings"]}"#;

const RULES: &str = "\
You are yogan's critic. A worker made the change on this branch; assume it is wrong and prove \
it. Never edit files.
- Look for inputs or states where it fails, acceptance criteria the diff doesn't satisfy, \
mismatches with the task, missing tests, unhandled errors, changes outside the task's crates, and \
money-handling hazards such as rounding, overflow and non-idempotent retries. A new type, \
endpoint, event or trait that no criterion requires is a major finding.
- Severity: blocker is wrong behaviour with reproducible evidence; major is a likely bug or a \
missing test for a stated requirement; minor is style, naming or a small improvement. An \
acceptance criterion the diff doesn't satisfy is always a blocker.
- Evidence for a blocker or major: a failing test or command you ran, or a call path traced from \
a real caller to the bug. Leave it empty when you have no proof; such findings are only listed \
as optional.
- Report every finding in the structured output, or an empty list if you find none.";

/// Reviews the task's branch in `slot` against `base`, in a fresh session with read access,
/// `cargo check`/`cargo test` and `git diff`/`git log`. `env` is the slot's environment.
pub fn run(
    task: &Task,
    slot: &Path,
    base: &str,
    cfg: &Config,
    env: &[(&str, String)],
    logs: &Path,
) -> Result<Findings> {
    let c = &cfg.critic;
    worker::check_effort(&c.effort)?;
    let read = ["Read", "Grep", "Glob"].map(String::from);
    let run = ["cargo check", "cargo test", "git diff", "git log"].map(|b| format!("Bash({b}:*)"));
    let allowed = [&read[..], &cfg.worker.read_tools[..], &run[..]].concat();
    fs::create_dir_all(logs)?;
    let append = |name: &str| {
        let path = logs.join(format!("{}.{name}", task.id));
        File::options().create(true).append(true).open(path)
    };
    let (mut log, err_log) = (append("critic.jsonl")?, append("stderr.log")?);

    let mut cmd = Command::new("claude");
    cmd.args(["--model", &c.model, "--effort", &c.effort]);
    if let Some(file) = &cfg.worker.mcp_config {
        cmd.args(["--mcp-config", file]);
    }
    cmd.arg("--allowedTools")
        .args(&allowed)
        .args(["--disallowedTools", "Edit", "Write", "NotebookEdit"])
        .args(["--json-schema", SCHEMA])
        .args(["--append-system-prompt", RULES])
        .current_dir(slot)
        .envs(env.iter().cloned());
    let mut report = None;
    worker::claude(&mut cmd, &prompt(task, base), &mut log, &err_log, |event| {
        if let Event::Result(r) = event {
            report = r.structured_output;
        }
        Ok(())
    })?;
    #[derive(Deserialize)]
    struct Report {
        findings: Vec<Finding>,
    }
    let report: Report =
        serde_json::from_value(report.context("the critic returned no findings")?)?;
    let (optional, findings) = report
        .findings
        .into_iter()
        .partition(|f| f.severity != Severity::Minor && f.evidence.trim().is_empty());
    Ok(Findings {
        findings,
        optional,
        error: None,
    })
}

fn prompt(task: &Task, base: &str) -> String {
    let mut p = format!(
        "Review this change.\n\nTask: {}\n\n{}\n",
        task.title, task.body
    );
    if !task.crates.is_empty() {
        p.push_str(&format!("\nCrates: {}\n", task.crates.join(", ")));
    }
    if !task.acceptance.is_empty() {
        p.push_str(&format!(
            "\nAcceptance criteria:\n- {}\n",
            task.acceptance.join("\n- ")
        ));
    }
    p.push_str(&format!(
        "\nThe change is `git diff {base}...HEAD`, its commits `git log {base}..HEAD`.\n"
    ));
    p
}
