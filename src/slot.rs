use anyhow::{Context, Result};
use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};

use crate::git;
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

/// Checks out a fresh `branch` from `base` (e.g. `origin/main`) in slot `n`, adding the
/// worktree on first use. Discards whatever the slot's previous task left behind.
pub fn prepare(repo: &Path, state: &Path, n: u32, branch: &str, base: &str) -> Result<PathBuf> {
    let dir = state.join("slots").join(n.to_string());
    git(repo, &["worktree", "prune"])?;
    git(repo, &["fetch", "--quiet", "origin"])?;
    if !dir.join(".git").exists() {
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
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocate_reuse_lock() {
        let root = std::env::temp_dir().join(format!("yogan-slot-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (repo, state) = (root.join("repo"), root.join("state"));
        fs::create_dir_all(&repo).unwrap();
        let origin = root.join("origin.git");
        let origin = origin.to_str().unwrap();
        let commit = [
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ];
        git(&root, &["init", "-q", "--bare", "-b", "main", origin]).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]).unwrap();
        fs::write(repo.join("a.txt"), "a").unwrap();
        git(&repo, &["add", "a.txt"]).unwrap();
        git(&repo, &[&commit[..], &["commit", "-qm", "init"]].concat()).unwrap();
        git(&repo, &["remote", "add", "origin", origin]).unwrap();
        git(&repo, &["push", "-q", "origin", "main"]).unwrap();

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
}
