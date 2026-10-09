//! The critic: a fresh, read-only session on another model that tries to break the change.

use std::fs::{self, File};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    /// The worker's reason when it fixed or disputed it.
    pub reply: Option<String>,
}

/// `findings/<id>.toml`.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Findings {
    /// Proven blockers and majors, and every minor.
    pub findings: Vec<Finding>,
    /// Blockers and majors without evidence: shown in Review, never sent back.
    pub optional: Vec<Finding>,
    /// Sent back and reported fixed by the worker; the next critic checks them.
    #[serde(default)]
    pub fixed: Vec<Finding>,
    /// Sent back and disputed by the worker: for the human, never argued with the critic.
    #[serde(default)]
    pub disputed: Vec<Finding>,
    /// Set aside by the human, whose reason is the reply; listed in the PR body.
    #[serde(default)]
    pub waived: Vec<Finding>,
    /// Why the critic didn't finish, if it didn't.
    pub error: Option<String>,
}

impl Findings {
    pub fn save(&self, state: &Path, id: &str) -> Result<()> {
        task::write_toml(&state.join("findings"), id, self)
    }

    /// The task's findings, or none if the critic hasn't run.
    pub fn load(state: &Path, id: &str) -> Result<Findings> {
        match fs::read_to_string(state.join(format!("findings/{id}.toml"))) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Findings::default()),
            text => Ok(toml::from_str(&text?)?),
        }
    }

    /// What the human can act on, as the Findings tab lists it: open, disputed, then optional.
    pub fn actionable(&self) -> impl Iterator<Item = &Finding> {
        (self.findings.iter())
            .chain(&self.disputed)
            .chain(&self.optional)
    }

    /// Takes the `i`th of `actionable` out of its list.
    fn take(&mut self, i: usize) -> Result<Finding> {
        let mut i = i;
        for list in [&mut self.findings, &mut self.disputed, &mut self.optional] {
            if i < list.len() {
                return Ok(list.remove(i));
            }
            i -= list.len();
        }
        anyhow::bail!("no such finding")
    }

    /// `x`: sets the `i`th actionable finding aside with the human's `reason`.
    pub fn waive(&mut self, i: usize, reason: &str) -> Result<()> {
        let mut f = self.take(i)?;
        f.reply = Some(reason.into());
        self.waived.push(f);
        Ok(())
    }

    /// Whether the `i`th of `actionable` is a disputed one.
    pub fn is_disputed(&self, i: usize) -> bool {
        let first = self.findings.len();
        (first..first + self.disputed.len()).contains(&i)
    }

    /// `r` on a disputed finding: the human sides with the critic. It moves to `fixed`, for the
    /// next critic to check once the worker has fixed it; returns it for the worker's prompt.
    pub fn uphold(&mut self, i: usize) -> Result<Finding> {
        anyhow::ensure!(self.is_disputed(i), "only a disputed finding can be upheld");
        let f = self.take(i)?;
        self.fixed.push(f.clone());
        Ok(f)
    }

    /// The blockers and majors to send back; unproven ones are in `optional` already.
    pub fn open(&self) -> Vec<Finding> {
        let blocking = |f: &&Finding| f.severity != Severity::Minor;
        self.findings.iter().filter(blocking).cloned().collect()
    }

    /// Files each of `open` as fixed or disputed by the worker's `reply` to `fix_prompt`; one it
    /// didn't answer counts as fixed, for the next critic to check.
    pub fn resolve(&mut self, open: Vec<Finding>, reply: Option<Value>) {
        #[derive(Deserialize)]
        struct Reply {
            findings: Vec<Resolution>,
        }
        #[derive(Deserialize)]
        struct Resolution {
            number: usize,
            status: String,
            reason: String,
        }
        let reply = reply.and_then(|r| serde_json::from_value::<Reply>(r).ok());
        let answers = reply.map(|r| r.findings).unwrap_or_default();
        self.findings.retain(|f| !open.contains(f));
        for (i, mut f) in open.into_iter().enumerate() {
            let answer = answers.iter().find(|a| a.number == i + 1);
            f.reply = answer.map(|a| a.reason.clone());
            match answer {
                Some(a) if a.status == "disputed" => self.disputed.push(f),
                _ => self.fixed.push(f),
            }
        }
    }
}

/// The structured reply a fix round ends with.
pub const REPLY: &str = r#"{"type":"object","properties":{"findings":{"type":"array","items":{"type":"object","properties":{"number":{"type":"integer"},"status":{"enum":["fixed","disputed"]},"reason":{"type":"string"}},"required":["number","status","reason"]}}},"required":["findings"]}"#;

/// Sends `open` back to the worker, numbered for its structured reply.
pub fn fix_prompt(open: &[Finding]) -> String {
    let mut p = "The critic found these problems. Fix each one and commit, or dispute it with a \
                 reason if you're sure it's wrong. End with structured output that lists every \
                 finding by number as fixed or disputed, with a short reason.\n"
        .to_string();
    for (i, f) in open.iter().enumerate() {
        p.push_str(&format!(
            "\n{}. [{:?}] {} - {}\n   Evidence: {}\n",
            i + 1,
            f.severity,
            f.location,
            f.claim,
            f.evidence
        ));
    }
    p
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
/// `cargo check`/`cargo test` and `git diff`/`git log`. `env` is the slot's environment. After
/// a fix round, `previous` holds what was fixed, to re-check, and disputed, to leave alone.
pub fn run(
    task: &Task,
    slot: &Path,
    base: &str,
    cfg: &Config,
    env: &[(&str, String)],
    logs: &Path,
    previous: &Findings,
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
    let prompt = prompt(task, base, previous);
    worker::claude(&mut cmd, &prompt, &mut log, &err_log, None, |event| {
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
        fixed: previous.fixed.clone(),
        disputed: previous.disputed.clone(),
        waived: previous.waived.clone(),
        error: None,
    })
}

fn prompt(task: &Task, base: &str, previous: &Findings) -> String {
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
    let list = |fs: &[Finding]| {
        let lines = fs.iter().map(|f| format!("- {} - {}", f.location, f.claim));
        lines.collect::<Vec<_>>().join("\n")
    };
    if !previous.fixed.is_empty() {
        p.push_str(&format!(
            "\nThe worker says it fixed these earlier findings. Check each one and report any \
             that isn't resolved. Then do a simplicity pass: code that can be removed, merged or \
             kept smaller, not a hunt for new bugs.\n{}\n",
            list(&previous.fixed)
        ));
    }
    let settled: Vec<_> = (previous.disputed.iter())
        .chain(&previous.waived)
        .cloned()
        .collect();
    if !settled.is_empty() {
        p.push_str(&format!(
            "\nThe worker disputes these or the human waived them, so don't report them again:\n{}\n",
            list(&settled)
        ));
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fix_round_files_findings_as_fixed_or_disputed() {
        let finding = |severity, claim: &str| Finding {
            severity,
            location: "src/lib.rs:1".into(),
            claim: claim.into(),
            evidence: "cargo test fails".into(),
            reply: None,
        };
        let mut findings = Findings {
            findings: vec![
                finding(Severity::Blocker, "a"),
                finding(Severity::Minor, "b"),
                finding(Severity::Major, "c"),
            ],
            ..Default::default()
        };
        let open = findings.open();
        assert_eq!(open.len(), 2, "minors stay in Review");
        let prompt = fix_prompt(&open);
        assert!(prompt.contains("1. [Blocker] src/lib.rs:1 - a") && prompt.contains("2. [Major]"));

        let reply = serde_json::json!({"findings": [
            {"number": 2, "status": "disputed", "reason": "c is intended"},
        ]});
        findings.resolve(open, Some(reply));
        assert_eq!(findings.findings, [finding(Severity::Minor, "b")]);
        // unanswered counts as fixed, for the next critic to check
        assert_eq!(findings.fixed[0].claim, "a");
        assert_eq!(findings.fixed[0].reply, None);
        assert_eq!(findings.disputed[0].reply.as_deref(), Some("c is intended"));

        // the human: uphold only a disputed one, waive any actionable one
        assert!(findings.uphold(0).is_err(), "b is open, not disputed");
        let upheld = findings.uphold(1).unwrap();
        assert_eq!((upheld.claim.as_str(), findings.fixed.len()), ("c", 2));
        findings.waive(0, "style only").unwrap();
        assert_eq!(findings.waived[0].reply.as_deref(), Some("style only"));
        assert_eq!(findings.actionable().count(), 0);
        assert!(findings.waive(0, "x").is_err());
    }
}
