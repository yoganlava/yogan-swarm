//! Checks a slot's work after Claude exits: a clean tree, new commits, then the `[gate]` steps
//! on the touched crates (or top-level dirs) only.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, ensure};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::config::Step;
use crate::git;
use crate::redact::redact;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub passed: bool,
}

/// Runs the gate in `slot` on the changes since `base`, writing all output to `log`.
/// Returns each check, and the failures' output for a fix round (empty when all passed).
pub fn run(
    slot: &Path,
    base: &str,
    steps: &[Step],
    env: &[(&str, String)],
    log: &Path,
) -> Result<(Vec<Check>, String)> {
    let mut log = File::create(log)?;
    let (mut checks, mut failures) = (Vec::new(), String::new());
    let mut record = |name: &str, passed: bool, output: &str| -> Result<()> {
        let output = redact(output);
        writeln!(
            log,
            "== {name}: {}\n{output}",
            if passed { "ok" } else { "FAILED" }
        )?;
        if !passed {
            // ponytail: last 100 lines per failure; the worker can't read the log outside its slot
            let lines: Vec<&str> = output.lines().collect();
            let tail = lines[lines.len().saturating_sub(100)..].join("\n");
            failures.push_str(&format!("## {name}\n{tail}\n\n"));
        }
        checks.push(Check {
            name: name.into(),
            passed,
        });
        Ok(())
    };

    // the copied .mcp.json and graft/ are untracked in every slot
    let dirty = git(
        slot,
        &["status", "--porcelain", "--", ".", ":!.mcp.json", ":!graft"],
    )?;
    record("clean tree", dirty.is_empty(), &dirty)?;
    let commits = git(slot, &["rev-list", "--count", &format!("{base}..HEAD")])?;
    record("new commits", commits != "0", "no commits since the base")?;

    let changed = git(slot, &["diff", "--name-only", &format!("{base}...HEAD")])?;
    let files: Vec<&str> = changed.lines().collect();
    let crates = if slot.join("Cargo.toml").exists() {
        crates(slot, &files, env)?
    } else {
        Vec::new()
    };
    let crates: Vec<String> = crates.iter().map(|c| format!("-p {c}")).collect();
    let paths = files.iter().filter_map(|f| f.split('/').next());
    let paths: Vec<&str> = paths.collect::<BTreeSet<_>>().into_iter().collect();
    let base_sha = git(slot, &["merge-base", base, "HEAD"])?;
    let head_sha = git(slot, &["rev-parse", "HEAD"])?;

    for step in steps {
        let empty = (step.run.contains("{crates}") && crates.is_empty())
            || (step.run.contains("{paths}") && paths.is_empty());
        if empty || !wanted(step, slot, base, &files)? {
            continue;
        }
        let cmd = step
            .run
            .replace("{crates}", &crates.join(" "))
            .replace("{paths}", &paths.join(" "));
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!("exec 2>&1\n{cmd}"))
            .current_dir(slot)
            .envs(env.iter().cloned())
            .env("BASE_SHA", &base_sha)
            .env("HEAD_SHA", &head_sha)
            .output()?;
        let output = String::from_utf8_lossy(&out.stdout);
        record(
            &step.name,
            out.status.success(),
            &format!("$ {cmd}\n{output}"),
        )?;
    }
    Ok((checks, failures))
}

/// A step runs when a changed path matches one of its `when_changed` globs, if any, and a
/// changed file's path or content matches its `when_files_contain`, if set.
fn wanted(step: &Step, slot: &Path, base: &str, files: &[&str]) -> Result<bool> {
    if !step.when_changed.is_empty() {
        let range = format!("{base}...HEAD");
        let globs: Vec<String> = step
            .when_changed
            .iter()
            .map(|g| format!(":(glob){g}"))
            .collect();
        let mut args = vec!["diff", "--name-only", &range, "--"];
        args.extend(globs.iter().map(String::as_str));
        if git(slot, &args)?.is_empty() {
            return Ok(false);
        }
    }
    let Some(pattern) = &step.when_files_contain else {
        return Ok(true);
    };
    let re = Regex::new(pattern).with_context(|| format!("gate step {}", step.name))?;
    Ok(files.iter().any(|f| {
        re.is_match(f) || fs::read_to_string(slot.join(f)).is_ok_and(|text| re.is_match(&text))
    }))
}

/// Workspace packages that own any of `files`.
fn crates(slot: &Path, files: &[&str], env: &[(&str, String)]) -> Result<Vec<String>> {
    let out = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(slot)
        .envs(env.iter().cloned())
        .output()?;
    let err = String::from_utf8_lossy(&out.stderr);
    ensure!(out.status.success(), "cargo metadata: {}", err.trim());
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let root = meta["workspace_root"]
        .as_str()
        .context("no workspace_root")?;
    let mut packages = Vec::new();
    for p in meta["packages"].as_array().into_iter().flatten() {
        let (Some(name), Some(manifest)) = (p["name"].as_str(), p["manifest_path"].as_str()) else {
            continue;
        };
        let dir = Path::new(manifest)
            .parent()
            .context("manifest has no dir")?;
        let dir = dir.strip_prefix(root).unwrap_or(dir);
        packages.push((name.to_string(), dir.to_string_lossy().into_owned()));
    }
    Ok(owners(files, &packages))
}

/// For each file, the package whose dir (relative to the workspace root) holds it most deeply.
fn owners(files: &[&str], packages: &[(String, String)]) -> Vec<String> {
    let mut found = BTreeSet::new();
    for file in files {
        let owner = packages
            .iter()
            .filter(|(_, dir)| dir.is_empty() || file.starts_with(&format!("{dir}/")))
            .max_by_key(|(_, dir)| dir.len());
        if let Some((name, _)) = owner {
            found.insert(name.clone());
        }
    }
    found.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_map_to_deepest_package() {
        let packages = [
            ("root".to_string(), String::new()),
            ("ledger".to_string(), "crates/ledger".to_string()),
            ("ledger-sys".to_string(), "crates/ledger/sys".to_string()),
            ("api".to_string(), "crates/api".to_string()),
        ];
        let files = [
            "crates/ledger/src/lib.rs",
            "crates/ledger/sys/build.rs",
            "crates/ledger-old/x.rs", // not under crates/ledger/
            "README.md",
        ];
        assert_eq!(owners(&files, &packages), ["ledger", "ledger-sys", "root"]);
        assert!(owners(&["docs/a.md"], &packages[1..]).is_empty());
    }

    #[test]
    fn steps_on_touched_crates_and_conditional() {
        let root = std::env::temp_dir().join(format!("yogan-gate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let manifest = |name: &str| {
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n")
        };
        let files = [
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"a\", \"b\"]\n".to_string(),
            ),
            (".gitignore", "Cargo.lock\n".to_string()),
            ("a/Cargo.toml", manifest("a")),
            ("a/src/lib.rs", String::new()),
            ("b/Cargo.toml", manifest("b")),
            ("b/src/lib.rs", String::new()),
        ];
        for (path, text) in &files {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        let id = [
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ];
        let commit = |msg: &str| {
            git(&root, &["add", "."]).unwrap();
            git(&root, &[&id[..], &["commit", "-qm", msg]].concat()).unwrap();
        };
        git(&root, &["init", "-q", "-b", "main"]).unwrap();
        commit("init");
        git(&root, &["branch", "base"]).unwrap();

        let step = |name: &str, run: &str, when: Option<&str>| Step {
            name: name.into(),
            run: run.into(),
            when_files_contain: when.map(Into::into),
            when_changed: Vec::new(),
        };
        let migration = Step {
            when_changed: vec!["a/**".into()],
            ..step("migration", "echo shas $BASE_SHA $HEAD_SHA", None)
        };
        let steps = [
            step("crates", "echo {crates}", None),
            step("paths", "echo {paths}", None),
            step("sqlx", "echo sqlx", Some(r"query(_as)?!")),
            step("fails", "echo broken; exit 1", Some("never matches")),
            migration,
        ];
        let log = root.with_extension("log");
        let run = || run(&root, "base", &steps, &[], &log).unwrap();

        // nothing committed yet
        let (checks, failures) = run();
        let passed: Vec<bool> = checks.iter().map(|c| c.passed).collect();
        assert_eq!(passed, [true, false]); // clean tree, new commits
        assert!(failures.contains("## new commits"), "{failures}");

        // a change to crate b runs only on b; the sqlx step waits for a query
        fs::write(root.join("b/src/lib.rs"), "pub fn f() {}\n").unwrap();
        commit("b");
        let (checks, failures) = run();
        assert_eq!(checks.len(), 4);
        assert!(checks.iter().all(|c| c.passed) && failures.is_empty());
        let logged = fs::read_to_string(&log).unwrap();
        assert!(logged.contains("$ echo -p b\n-p b\n"), "{logged}");
        assert!(logged.contains("$ echo b\nb\n"), "{logged}");
        assert!(!logged.contains("sqlx"), "{logged}");
        assert!(!logged.contains("migration"), "{logged}");

        // a query in a's content triggers it; an uncommitted file fails the clean tree
        fs::write(root.join("a/src/lib.rs"), "fn q() { query_as!(x) }\n").unwrap();
        commit("a");
        fs::write(root.join("stray.txt"), "").unwrap();
        let (checks, failures) = run();
        let logged = fs::read_to_string(&log).unwrap();
        assert!(logged.contains("$ echo -p a -p b\n"), "{logged}");
        assert!(logged.contains("== sqlx: ok"), "{logged}");
        let shas = format!(
            "shas {} {}\n",
            git(&root, &["merge-base", "base", "HEAD"]).unwrap(),
            git(&root, &["rev-parse", "HEAD"]).unwrap()
        );
        assert!(logged.contains(&shas), "{logged}");
        assert_eq!(
            checks[0],
            Check {
                name: "clean tree".into(),
                passed: false
            }
        );
        assert!(failures.contains("stray.txt"), "{failures}");

        fs::remove_dir_all(&root).unwrap();
        fs::remove_file(&log).unwrap();
    }
}
