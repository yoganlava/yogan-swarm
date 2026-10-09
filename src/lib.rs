use std::path::Path;
use std::process::Command;

use anyhow::{Result, bail};

pub mod config;
pub mod redact;
pub mod slot;
pub mod stream;
pub mod task;

/// Runs `git -C dir args…` and returns trimmed stdout; a failure carries git's stderr.
pub(crate) fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("git {}: {}", args.join(" "), err.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
