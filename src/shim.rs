//! The `cargo` first on every slot's PATH: a symlink to yogan, which acts as the shim when
//! run as `cargo`. Builds hold one of `max_cargo` machine-wide permits while they compile.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, TryLockError};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::time::Duration;

use anyhow::{Context, Result};

/// Subcommands that never compile, so they skip the queue.
const READ_ONLY: &[&str] = &[
    "metadata",
    "tree",
    "locate-project",
    "pkgid",
    "read-manifest",
    "verify-project",
    "version",
    "help",
    "fmt",
    "search",
    "info",
    "fetch",
    "generate-lockfile",
    "clean",
];

/// Variables that put the shim first on slot `n`'s PATH, linking `<state>/bin/cargo` to `exe`.
pub fn env(
    state: &Path,
    n: u32,
    exe: &Path,
    permits: &Path,
    max_cargo: u32,
    wrapper: Option<&Path>,
) -> Result<Vec<(&'static str, String)>> {
    let bin = state.join("bin");
    fs::create_dir_all(&bin)?;
    fs::create_dir_all(permits)?;
    let link = bin.join("cargo");
    if fs::read_link(&link).ok().as_deref() != Some(exe) {
        let tmp = bin.join(format!("cargo.{}", std::process::id()));
        let _ = fs::remove_file(&tmp);
        std::os::unix::fs::symlink(exe, &tmp)?;
        fs::rename(&tmp, &link)?;
    }
    let cores = std::thread::available_parallelism().map_or(1, |c| c.get() as u32);
    let path = std::env::var("PATH").unwrap_or_default();
    let lock = state.join(format!("slots/{n}.build"));
    let queue = queue(state, n);
    let mut env = vec![
        ("PATH", format!("{}:{path}", bin.display())),
        ("YOGAN_SHIM_DIR", bin.display().to_string()),
        ("YOGAN_PERMITS", permits.display().to_string()),
        ("YOGAN_MAX_CARGO", max_cargo.to_string()),
        ("YOGAN_BUILD_LOCK", lock.display().to_string()),
        ("YOGAN_QUEUE", queue.display().to_string()),
        (
            "CARGO_BUILD_JOBS",
            (cores / max_cargo.max(1)).max(1).to_string(),
        ),
    ];
    if let Some(w) = wrapper {
        env.push(("YOGAN_CARGO_WRAPPER", w.display().to_string()));
    }
    Ok(env)
}

/// Held shared by slot `n`'s builds while they wait for a permit, so the wait isn't a stall.
pub(crate) fn queue(state: &Path, n: u32) -> PathBuf {
    state.join(format!("slots/{n}.queue"))
}

/// The shim's main: returns cargo's exit code, or execs into it.
pub fn run(args: Vec<OsString>) -> Result<i32> {
    let var =
        |k: &str| std::env::var_os(k).with_context(|| format!("{k} unset: not in a yogan slot"));
    let shim_dir = var("YOGAN_SHIM_DIR")?;
    // Everything below sees PATH without the shim, so a wrapper's own `cargo` call is the real one.
    let path = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .filter(|p| p.as_os_str() != shim_dir)
        .collect::<Vec<_>>();
    let path = std::env::join_paths(path)?;

    // ponytail: the first non-flag arg is the subcommand, so a flag value before it
    // (`--config x build`) is misread; fine for the commands agents run
    let sub = args
        .iter()
        .position(|a| !a.to_string_lossy().starts_with(['-', '+']));
    let compiles = sub.is_some_and(|i| !READ_ONLY.contains(&&*args[i].to_string_lossy()));
    if !compiles {
        return Err(Command::new("cargo")
            .args(&args)
            .env("PATH", &path)
            .exec()
            .into());
    }
    let program = std::env::var_os("YOGAN_CARGO_WRAPPER").unwrap_or_else(|| "cargo".into());
    let cargo = |args: &[OsString]| {
        let mut c = Command::new(&program);
        c.args(args).env("PATH", &path);
        c
    };

    let _slot = match std::env::var_os("YOGAN_BUILD_LOCK") {
        Some(p) => {
            let f = File::create(p)?;
            f.lock_shared()?; // cleaning holds it exclusively
            Some(f)
        }
        None => None,
    };
    let max: u32 = var("YOGAN_MAX_CARGO")?.to_string_lossy().parse()?;
    let queue = std::env::var_os("YOGAN_QUEUE");
    let permit = permit(Path::new(&var("YOGAN_PERMITS")?), max, queue.as_deref())?;

    let i = sub.unwrap_or_default();
    if args[i] == "test" && !args.iter().any(|a| a == "--no-run") {
        // compile under the permit, run the test binaries outside it
        let mut compile = args.clone();
        compile.insert(i + 1, "--no-run".into());
        let status = cargo(&compile).status()?;
        drop(permit);
        if !status.success() {
            return Ok(code(status));
        }
        return Err(cargo(&args).exec().into());
    }
    Ok(code(cargo(&args).status()?))
}

// ponytail: polls every 200 ms, no FIFO order; add a queue file if builds starve
fn permit(dir: &Path, max: u32, queue: Option<&OsStr>) -> Result<File> {
    let mut told = false;
    let mut _queued = None;
    loop {
        for k in 0..max {
            let f = File::create(dir.join(format!("{k}.lock")))?;
            match f.try_lock() {
                Ok(()) => return Ok(f),
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(e)) => return Err(e.into()),
            }
        }
        if !told {
            eprintln!("⧗ waiting for build slot");
            told = true;
            if let Some(q) = queue {
                let f = File::create(q)?;
                f.lock_shared()?;
                _queued = Some(f);
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}
