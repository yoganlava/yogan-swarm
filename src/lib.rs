use std::path::Path;
use std::process::Command;

use anyhow::{Result, bail};

pub mod config;
pub mod critic;
pub mod gate;
pub mod lead;
pub mod pr;
pub mod redact;
pub mod sched;
pub mod shim;
pub mod slot;
pub mod stream;
pub mod task;
pub mod tui;
pub mod worker;

/// Runs `git -C dir args…` and returns trimmed stdout; a failure carries git's stderr.
pub(crate) fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("git {}: {}", args.join(" "), err.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
