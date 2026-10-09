//! PR drafts and opening the PR. Drafting never pushes; only `open` does, after approval.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, ensure};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::config::Pr;
use crate::critic::Findings;
use crate::git;
use crate::task::Task;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Draft {
    pub title: String,
    pub body: String,
    /// The branch head it was written for; any other head makes it stale.
    pub head: String,
    /// Why the title fails the repo's title check, if it does.
    pub problem: Option<String>,
}

/// Has the `[pr]` model draft a title and body for the task's branch in `slot`, then appends
/// the review summary from `findings`. With an instruction, it redrafts from the same inputs
/// plus the current draft.
pub fn draft(
    task: &Task,
    slot: &Path,
    base: &str,
    cfg: &Pr,
    instruction: Option<&str>,
    findings: &Findings,
) -> Result<Draft> {
    let range = format!("{base}...HEAD");
    let stat = git(slot, &["diff", "--stat", &range])?;
    let migration = migration(slot, base)?;
    let ticket = task
        .ticket
        .as_ref()
        .map_or("[NO-TICKET]".into(), |t| format!("[{t}]"));
    let mut prompt = format!(
        "Write the pull request title and description for this change. Reply with only the \
         title on the first line, a blank line, then the description in Markdown. Use no tools \
         and add no generated-by footer.\n\n\
         Style: {}\n\nTask: {}\n\n{}\n",
        cfg.style, task.title, task.body
    );
    if !task.acceptance.is_empty() {
        prompt.push_str(&format!(
            "\nAcceptance criteria, which the description lists, each with whether the change \
             meets it:\n- {}\n",
            task.acceptance.join("\n- ")
        ));
    }
    let open: Vec<_> = (findings.findings.iter())
        .chain(&findings.optional)
        .collect();
    if !open.is_empty() {
        let open = open
            .iter()
            .map(|f| format!("- {}: {}", f.location, f.claim));
        prompt.push_str(&format!(
            "\nReview findings still open, which may mean a criterion isn't met:\n{}\n\
             yogan appends its own review summary, so don't write one.\n",
            open.collect::<Vec<_>>().join("\n")
        ));
    }
    if let Some(summary) = &task.summary {
        prompt.push_str(&format!("\nThe worker's summary:\n{summary}\n"));
    }
    prompt.push_str(&format!("\nDiffstat:\n{stat}\n"));
    if let Some(format) = &cfg.title_format {
        prompt.push_str(&format!(
            "\nTitle format: {format}, with one of these types: {}. Use the type `migration` \
             exactly when migration SQL changes ({}). End the title with {ticket}.\n",
            cfg.types.join(", "),
            if migration { "it does" } else { "it doesn't" },
        ));
    }
    if task.ticket.is_none() {
        prompt.push_str("\nThere is no ticket; say so in one line of the description.\n");
    }
    if let Ok(template) = fs::read_to_string(slot.join(".github/pull_request_template.md")) {
        prompt.push_str(&format!("\nFollow the repo's PR template:\n{template}\n"));
    }
    if let (Some(instruction), Some(current)) = (instruction, &task.pr_draft) {
        prompt.push_str(&format!(
            "\nThe current draft:\n{}\n\n{}\n\nRedraft it: {instruction}. Work only from the \
             inputs above; add no new claims.\n",
            current.title, current.body
        ));
    }

    let mut draft = ask(slot, cfg, &prompt)?;
    if let Err(problem) = check_title(&draft.0, &cfg.types, migration) {
        // one retry with the reason; anything left is shown in the preview to fix with `e`
        let retry = format!(
            "{prompt}\nYour title `{}` was rejected: {problem}. Fix it.",
            draft.0
        );
        draft = ask(slot, cfg, &retry)?;
    }
    let (title, body) = (clean(&draft.0), clean(&with_review(&draft.1, findings)));
    Ok(Draft {
        problem: check_title(&title, &cfg.types, migration).err(),
        title,
        body,
        head: git(slot, &["rev-parse", "HEAD"])?,
    })
}

/// One `[pr]` model turn; returns (title, body).
fn ask(slot: &Path, cfg: &Pr, prompt: &str) -> Result<(String, String)> {
    let out = Command::new("claude")
        .args(["-p", prompt, "--output-format", "json"])
        .args(["--model", &cfg.model, "--effort", &cfg.effort])
        .args(["--setting-sources", "project", "--strict-mcp-config"])
        .current_dir(slot)
        .env_remove("CLAUDE_CODE_EFFORT_LEVEL")
        .stdin(Stdio::null())
        .output()
        .context("starting claude for the PR draft")?;
    ensure!(
        out.status.success(),
        "PR draft: claude exited with {}",
        out.status
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let text = json["result"]
        .as_str()
        .context("PR draft: no result")?
        .trim();
    let (title, body) = text.split_once('\n').unwrap_or((text, ""));
    Ok((title.trim().into(), body.trim().into()))
}

/// Whether the branch changes migration SQL.
pub fn migration(slot: &Path, base: &str) -> Result<bool> {
    let files = git(slot, &["diff", "--name-only", &format!("{base}...HEAD")])?;
    Ok(files
        .lines()
        .any(|f| f.contains("migrations/") && f.ends_with(".sql")))
}

/// `type(scope)!: desc [KEY-1][KEY-2]`: a type from `types`, an optional scope and `!`, then
/// one or more tickets (or `[NO-TICKET]`) last; `migration` exactly when migration SQL changes.
/// No `types` means the repo has no title rule.
pub fn check_title(title: &str, types: &[String], migration: bool) -> Result<(), String> {
    if types.is_empty() {
        return Ok(());
    }
    // the description can't end in `]`, so tickets are only the concatenated group at the end
    let re = Regex::new(
        r"^([a-z]+)(\([^()\s]+\))?!?: (\S|\S.*[^\]\s]) ((\[[A-Z][A-Z0-9]*-\d+\])+|\[NO-TICKET\])$",
    )
    .expect("valid regex");
    let caps = re
        .captures(title)
        .ok_or("expected `type(scope): description [TICKET-1]`")?;
    let kind = &caps[1];
    if !types.iter().any(|t| t == kind) {
        return Err(format!("`{kind}` is not one of {}", types.join(", ")));
    }
    if (kind == "migration") != migration {
        return Err("use the `migration` type exactly when migration SQL changes".into());
    }
    Ok(())
}

/// `body` ending in yogan's Review section, which replaces any the draft carried over: what the
/// critic found, fixed, disputed and waived, with each dispute's and waiver's reason.
pub fn with_review(body: &str, f: &Findings) -> String {
    let body = body.split("**Review**").next().unwrap_or(body).trim_end();
    let open = f.findings.len() + f.optional.len();
    let counts = [
        (f.fixed.len(), "fixed"),
        (f.disputed.len(), "disputed"),
        (f.waived.len(), "waived"),
        (open, "open"),
    ];
    let found: usize = counts.iter().map(|(n, _)| n).sum();
    let mut review = match &f.error {
        Some(e) => format!("The critic didn't finish: {e}\n"),
        None if found == 0 => return body.to_string(),
        None => {
            let parts = counts.iter().filter(|(n, _)| *n > 0);
            let parts: Vec<_> = parts.map(|(n, what)| format!("{n} {what}")).collect();
            format!("The critic found {found}: {}.\n", parts.join(", "))
        }
    };
    for (what, list, who) in [
        ("Disputed", &f.disputed, "Worker"),
        ("Waived", &f.waived, "Reason"),
    ] {
        for x in list {
            let why = x.reply.as_deref().unwrap_or("none given");
            review.push_str(&format!(
                "\n- {what}: `{}` {} {who}: {why}",
                x.location, x.claim
            ));
        }
    }
    format!("{body}\n\n**Review**\n\n{}\n", review.trim_end())
}

/// Em and en dashes never reach git or gh.
pub fn clean(text: &str) -> String {
    text.replace(" — ", ", ").replace(['—', '–'], "-")
}

/// Pushes the branch with the user's own git credentials and opens the PR with `gh`.
/// Returns the PR's URL.
pub fn open(task: &Task, slot: &Path, base: &str, as_draft: bool) -> Result<String> {
    let draft = task.pr_draft.as_ref().context("no PR draft")?;
    git(slot, &["push", "--quiet", "-u", "origin", &task.branch])?;
    let base = base.strip_prefix("origin/").unwrap_or(base);
    let mut gh = Command::new("gh");
    gh.args(["pr", "create", "--base", base, "--head", &task.branch])
        .args([
            "--title",
            &clean(&draft.title),
            "--body",
            &clean(&draft.body),
        ])
        .current_dir(slot);
    if as_draft {
        gh.arg("--draft");
    }
    let out = gh.output().context("starting gh")?;
    let err = String::from_utf8_lossy(&out.stderr);
    ensure!(out.status.success(), "gh pr create: {}", err.trim());
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::critic::{Finding, Severity};

    #[test]
    fn title_check() {
        let types: Vec<String> = ["feat", "fix", "migration"].map(Into::into).into();
        let ok = |t: &str| check_title(t, &types, false);
        assert!(ok("feat(ledger): reject negative max_delay [CC-687]").is_ok());
        assert!(ok("fix: retry webhook sends [CC-1][OPS-22]").is_ok());
        assert!(ok("feat(api)!: drop v1 routes [NO-TICKET]").is_ok());
        assert!(ok("feat!: drop v1 routes [CC-2]").is_ok());

        assert!(
            ok("chore: bump deps [CC-1]")
                .unwrap_err()
                .contains("not one of")
        );
        assert!(ok("feat(ledger) reject [CC-1]").is_err()); // no colon
        assert!(ok("feat: [CC-1] reject negatives").is_err()); // tickets not last
        assert!(ok("feat: reject negatives").is_err()); // no ticket
        assert!(ok("feat: reject negatives [CC-1] [CC-2]").is_err()); // tickets not concatenated
        assert!(ok("feat: x [NO-TICKET][CC-1]").is_err());
        assert!(ok("Feat: reject negatives [CC-1]").is_err());

        // the migration type goes with migration SQL, and only with it
        assert!(check_title("migration: add limits [CC-3]", &types, true).is_ok());
        assert!(ok("migration: add limits [CC-3]").is_err());
        assert!(check_title("feat: add limits [CC-3]", &types, true).is_err());

        assert!(check_title("anything goes", &[], false).is_ok());

        // the review summary replaces one a redraft carried over
        let f = |claim: &str, reply: Option<&str>| Finding {
            severity: Severity::Major,
            location: "src/a.rs:1".into(),
            claim: claim.into(),
            evidence: String::new(),
            reply: reply.map(Into::into),
        };
        let findings = Findings {
            fixed: vec![f("x", None), f("y", None)],
            disputed: vec![f("no test", Some("covered by z"))],
            waived: vec![f("rename", Some("matches the API"))],
            ..Default::default()
        };
        let body = with_review("What and why.\n\n**Review**\n\nstale", &findings);
        assert_eq!(
            body,
            "What and why.\n\n**Review**\n\nThe critic found 4: 2 fixed, 1 disputed, 1 waived.\n\n\
             - Disputed: `src/a.rs:1` no test Worker: covered by z\n\
             - Waived: `src/a.rs:1` rename Reason: matches the API\n"
        );
        assert_eq!(with_review("Body.", &Findings::default()), "Body.");
        assert_eq!(clean("Fix — retries – fast—now"), "Fix, retries - fast-now");
    }
}
