use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};

use crate::config::Ports;
use crate::git;
use crate::redact::redact;
use crate::task::Task;

/// Lowest slot in `1..=count` that no task holds and no process has locked.
/// The slot stays locked until the returned file is dropped.
pub fn claim(state: &Path, tasks: &[Task], count: u32) -> Result<Option<(u32, File)>> {
    fs::create_dir_all(state.join("slots"))?;
    for n in 1..=count {
        if tasks.iter().any(|t| t.slot == Some(n)) {
            continue;
        }
        let file = File::create(state.join(format!("slots/{n}.lock")))?;
        match file.try_lock() {
            Ok(()) => return Ok(Some((n, file))),
            Err(TryLockError::WouldBlock) => continue,
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }
    }
    Ok(None)
}

/// Checks out a fresh `branch` from `base` (e.g. `origin/main`) in slot `n`, adding and
/// seeding the worktree on first use. Discards whatever the slot's previous task left behind.
pub fn prepare(repo: &Path, state: &Path, n: u32, branch: &str, base: &str) -> Result<PathBuf> {
    let dir = state.join("slots").join(n.to_string());
    git(repo, &["worktree", "prune"])?;
    git(repo, &["fetch", "--quiet", "origin"])?;
    let fresh = !dir.join(".git").exists();
    if fresh {
        let path = dir.to_str().context("slot path is not UTF-8")?;
        git(
            repo,
            &["worktree", "add", "--quiet", "--detach", path, base],
        )?;
    }
    git(
        &dir,
        &["checkout", "--quiet", "--force", "-B", branch, base],
    )?;
    git(&dir, &["clean", "-fdq"])?;
    if fresh {
        seed(repo, &dir)?;
    }
    Ok(dir)
}

/// Variables that slot scripts, Claude, the gate and the critic all run with.
pub fn env(
    repo: &Path,
    slot: &Path,
    n: u32,
    task: &Task,
    ports: Option<&Ports>,
) -> Vec<(&'static str, String)> {
    let mut env = vec![
        ("YOGAN_ROOT", repo.display().to_string()),
        ("YOGAN_SLOT", n.to_string()),
        ("YOGAN_SLOT_DIR", slot.display().to_string()),
        ("YOGAN_TASK_ID", task.id.clone()),
        ("YOGAN_CRATES", task.crates.join(" ")),
    ];
    if let Some(p) = ports {
        env.push(("YOGAN_PORT_BASE", (p.base + n * p.per_slot).to_string()));
        env.push(("YOGAN_PORT_COUNT", p.per_slot.to_string()));
    }
    env
}

/// Per-task setup: copies `.mcp.json` and `graft/` (both untracked) from the main checkout
/// when the slot lacks them, then runs `[scripts] setup` if there is one.
pub fn setup(
    repo: &Path,
    slot: &Path,
    env: &[(&str, String)],
    script: Option<&str>,
    log: &Path,
) -> Result<()> {
    let missing: Vec<PathBuf> = [".mcp.json", "graft"]
        .into_iter()
        .filter(|name| repo.join(name).exists() && !slot.join(name).exists())
        .map(|name| repo.join(name))
        .collect();
    clone_into(&missing, slot)?;
    match script {
        Some(cmd) => run_script(cmd, slot, env, log),
        None => Ok(()),
    }
}

/// Runs a `[scripts]` command with `sh` in the slot; stdout and stderr, redacted, go to `log`.
pub fn run_script(cmd: &str, slot: &Path, env: &[(&str, String)], log: &Path) -> Result<()> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!("exec 2>&1\n{cmd}"))
        .current_dir(slot)
        .envs(env.iter().cloned())
        .output()?;
    if let Some(dir) = log.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(log, redact(&String::from_utf8_lossy(&out.stdout)))?;
    ensure!(
        out.status.success(),
        "`{cmd}` failed ({}), log: {}",
        out.status,
        log.display()
    );
    Ok(())
}

/// Cargo repos only: clones the main checkout's target dir into `<slot>/target` without
/// incremental caches (rustc never reuses them after a move), then copies mtimes so Cargo's
/// fingerprints still match.
fn seed(repo: &Path, slot: &Path) -> Result<()> {
    if !repo.join("Cargo.toml").exists() {
        return Ok(());
    }
    // ponytail: assumes the default `<repo>/target`; ask `cargo metadata` if a repo sets target-dir
    let main_target = repo.join("target");
    if main_target.is_dir() {
        clone_tree(&main_target, &slot.join("target"), 0)?;
    }
    copy_mtimes(repo, slot)
}

/// Clones `src` to `dst` copy-on-write, minus `incremental` dirs, which sit at most two levels
/// down (`debug/incremental`, `<triple>/debug/incremental`).
fn clone_tree(src: &Path, dst: &Path, depth: u8) -> Result<()> {
    fs::create_dir_all(dst)?;
    let mut whole = Vec::new();
    for entry in fs::read_dir(src)? {
        let path = entry?.path();
        let name = path
            .file_name()
            .context("dir entry has no name")?
            .to_owned();
        if name == "incremental" {
            continue;
        }
        if depth < 2 && holds_incremental(&path) {
            clone_tree(&path, &dst.join(name), depth + 1)?;
        } else {
            whole.push(path);
        }
    }
    clone_into(&whole, dst)
}

/// `cp -a`, copy-on-write where the filesystem supports it.
fn clone_into(srcs: &[PathBuf], dst: &Path) -> Result<()> {
    if srcs.is_empty() {
        return Ok(());
    }
    let clone = if cfg!(target_os = "macos") {
        "-c"
    } else {
        "--reflink=auto"
    };
    let status = Command::new("cp")
        .args(["-a", clone])
        .args(srcs)
        .arg(dst)
        .status()?;
    ensure!(status.success(), "cloning into {} failed", dst.display());
    Ok(())
}

fn holds_incremental(dir: &Path) -> bool {
    dir.join("incremental").is_dir()
        || fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| e.path().join("incremental").is_dir())
}

/// A fresh worktree's files are newer than every cloned fingerprint. Gives each tracked file
/// the main checkout's mtime, but only when both hold the same blob and the main copy is
/// unmodified; anything else keeps its new mtime, so Cargo rebuilds it.
fn copy_mtimes(repo: &Path, slot: &Path) -> Result<()> {
    let main = blobs(repo)?;
    let slot_files = blobs(slot)?;
    let dirty = git(repo, &["diff", "--name-only", "-z"])?;
    let dirty: HashSet<&str> = dirty.split('\0').collect();
    for (path, blob) in &slot_files {
        if main.get(path) == Some(blob) && !dirty.contains(path.as_str()) {
            copy_mtime(&repo.join(path), &slot.join(path))?;
        }
    }
    // `rerun-if-changed=<dir>` also reads directory mtimes; a directory only gets the main
    // checkout's mtime when both list the same entries, so an added or removed file still shows.
    let dirs: HashSet<&Path> = slot_files
        .keys()
        .flat_map(|p| Path::new(p).ancestors().skip(1))
        .collect();
    for dir in dirs {
        let (from, to) = (repo.join(dir), slot.join(dir));
        if entries(&from).is_some_and(|e| Some(e) == entries(&to)) {
            copy_mtime(&from, &to)?;
        }
    }
    Ok(())
}

fn copy_mtime(from: &Path, to: &Path) -> Result<()> {
    let mtime = fs::metadata(from)?.modified()?;
    File::open(to)?.set_modified(mtime)?;
    Ok(())
}

fn entries(dir: &Path) -> Option<BTreeSet<OsString>> {
    let names = fs::read_dir(dir).ok()?.map(|e| e.map(|e| e.file_name()));
    names.collect::<std::io::Result<_>>().ok()
}

/// Tracked regular files (no symlinks or submodules) and their index blob ids.
fn blobs(dir: &Path) -> Result<HashMap<String, String>> {
    let out = git(dir, &["ls-files", "-s", "-z"])?;
    Ok(out
        .split('\0')
        .filter_map(|entry| {
            let (meta, path) = entry.split_once('\t')?;
            let mut meta = meta.split(' ');
            let (mode, blob) = (meta.next()?, meta.next()?);
            mode.starts_with("100")
                .then(|| (path.to_string(), blob.to_string()))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `<root>/repo` with `files` committed on `main` and pushed to a bare `<root>/origin.git`.
    fn repo_with_origin(name: &str, files: &[(&str, &str)]) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("yogan-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        let origin = root.join("origin.git");
        let origin = origin.to_str().unwrap();
        git(&root, &["init", "-q", "--bare", "-b", "main", origin]).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]).unwrap();
        for (path, text) in files {
            let path = repo.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        git(&repo, &["add", "."]).unwrap();
        let id = [
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ];
        git(&repo, &[&id[..], &["commit", "-qm", "init"]].concat()).unwrap();
        git(&repo, &["remote", "add", "origin", origin]).unwrap();
        git(&repo, &["push", "-q", "origin", "main"]).unwrap();
        (root, repo)
    }

    #[test]
    fn allocate_reuse_lock() {
        let (root, repo) = repo_with_origin("slot", &[("a.txt", "a")]);
        let state = root.join("state");

        // locks: a held lock skips the slot, and a full house returns None
        let (n, lock) = claim(&state, &[], 2).unwrap().unwrap();
        assert_eq!(n, 1);
        let (n, _lock2) = claim(&state, &[], 2).unwrap().unwrap();
        assert_eq!(n, 2);
        assert!(claim(&state, &[], 2).unwrap().is_none());

        let slot = prepare(&repo, &state, 1, "u/first", "origin/main").unwrap();
        assert_eq!(
            git(&slot, &["branch", "--show-current"]).unwrap(),
            "u/first"
        );
        assert!(slot.join("a.txt").exists());
        fs::write(slot.join("junk.txt"), "left over").unwrap();

        // a task holding slot 2 keeps it taken even without a lock
        drop(lock);
        let holder = Task {
            slot: Some(2),
            ..Default::default()
        };
        assert_eq!(claim(&state, &[holder], 2).unwrap().unwrap().0, 1);

        // reuse: same worktree, new branch, previous task's leftovers gone
        let again = prepare(&repo, &state, 1, "u/second", "origin/main").unwrap();
        assert_eq!(again, slot);
        assert_eq!(
            git(&slot, &["branch", "--show-current"]).unwrap(),
            "u/second"
        );
        assert!(!slot.join("junk.txt").exists());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn setup_env_script_and_log() {
        let root = std::env::temp_dir().join(format!("yogan-scripts-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (repo, slot) = (root.join("repo"), root.join("slot"));
        fs::create_dir_all(repo.join("graft")).unwrap();
        fs::create_dir_all(&slot).unwrap();
        fs::write(repo.join(".mcp.json"), "{}").unwrap();
        fs::write(repo.join("graft/INDEX.md"), "index").unwrap();
        let task = Task {
            id: "t7".into(),
            crates: vec!["ledger".into(), "api".into()],
            ..Default::default()
        };
        let ports = Ports {
            base: 1000,
            per_slot: 80,
        };
        let env = env(&repo, &slot, 2, &task, Some(&ports));
        let log = root.join("logs/t7.setup.log");

        let secret = format!("ghp_{}", "aB3".repeat(8));
        let script = format!(
            "echo \"$YOGAN_TASK_ID $YOGAN_SLOT $YOGAN_PORT_BASE $YOGAN_PORT_COUNT $YOGAN_CRATES\"\n\
             echo {secret} >&2"
        );
        setup(&repo, &slot, &env, Some(&script), &log).unwrap();
        let logged = fs::read_to_string(&log).unwrap();
        assert_eq!(logged, "t7 2 1160 80 ledger api\n[REDACTED]\n");
        assert!(slot.join(".mcp.json").exists());
        assert!(slot.join("graft/INDEX.md").exists());

        let err = setup(&repo, &slot, &env, Some("echo db down; exit 3"), &log).unwrap_err();
        assert!(err.to_string().contains("t7.setup.log"), "{err}");
        assert_eq!(fs::read_to_string(&log).unwrap(), "db down\n");

        // no [scripts]: nothing runs, the last log stays as it was
        setup(&repo, &slot, &env, None, &log).unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), "db down\n");

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn seeded_slot_rebuilds_nothing() {
        let manifest = "[package]\nname = \"seedling\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
        let files = [
            ("Cargo.toml", manifest),
            ("src/lib.rs", "pub fn f() {}\n"),
            (".gitignore", "/target\n"),
            // a directory dependency, like fuse-os db-sys's `migrations`
            (
                "build.rs",
                "fn main() { println!(\"cargo:rerun-if-changed=assets\"); }\n",
            ),
            ("assets/a.txt", "a"),
        ];
        let (root, repo) = repo_with_origin("seed", &files);
        let build = |dir: &Path| {
            let out = Command::new("cargo")
                .current_dir(dir)
                .env("CARGO_TARGET_DIR", dir.join("target"))
                .args(["build", "--offline"])
                .output()
                .unwrap();
            assert!(out.status.success());
            String::from_utf8(out.stderr).unwrap()
        };
        assert!(build(&repo).contains("Compiling seedling"));
        assert!(repo.join("target/debug/incremental").is_dir());

        let state = root.join("state");
        let slot = prepare(&repo, &state, 1, "u/a", "origin/main").unwrap();
        assert!(!slot.join("target/debug/incremental").exists());
        assert!(!build(&slot).contains("Compiling"), "seeded slot rebuilt");

        // an untracked file in a main-checkout dir keeps that dir's fresh mtime in the slot
        fs::write(repo.join("assets/new.txt"), "untracked").unwrap();
        let slot = prepare(&repo, &state, 2, "u/b", "origin/main").unwrap();
        assert!(build(&slot).contains("Compiling seedling"));
        fs::remove_file(repo.join("assets/new.txt")).unwrap();

        // a file the main checkout has modified keeps its fresh mtime, so the slot rebuilds
        fs::write(repo.join("src/lib.rs"), "pub fn f() { /* local edit */ }\n").unwrap();
        let slot = prepare(&repo, &state, 3, "u/c", "origin/main").unwrap();
        assert!(build(&slot).contains("Compiling seedling"));

        fs::remove_dir_all(&root).unwrap();
    }
}
