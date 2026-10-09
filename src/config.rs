use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use toml::{Table, Value};

const DEFAULTS: &str = include_str!("defaults.toml");

#[derive(Debug, Deserialize)]
pub struct Config {
    /// Prefixed to every task branch, e.g. `udeshya/`.
    #[serde(default)]
    pub branch_prefix: String,
    pub lead: Role,
    pub ask: Ask,
    pub pr: Pr,
    pub critic: Critic,
    pub worker: Worker,
    pub ports: Option<Ports>,
    pub scripts: Option<Scripts>,
    pub build: Build,
    pub cargo: Option<Cargo>,
    pub gate: Gate,
    pub watch: Watch,
    pub disk: Disk,
    pub tui: Tui,
    pub notify: Notify,
}

#[derive(Debug, Deserialize)]
pub struct Tui {
    /// Capture the mouse: click selects, the wheel scrolls.
    pub mouse: bool,
}

#[derive(Debug, Deserialize)]
pub struct Notify {
    /// Run with `sh` when a task reaches Review; empty runs nothing.
    pub on_review_ready: String,
}

/// Slot cleanup thresholds.
#[derive(Debug, Deserialize)]
pub struct Disk {
    /// Per slot, measured as divergence from the seed.
    pub max_target_gb: u64,
    pub min_free_gb: u64,
}

#[derive(Debug, Deserialize)]
pub struct Pr {
    pub model: String,
    pub effort: String,
    /// Open PRs as drafts.
    pub draft: bool,
    pub style: String,
    /// e.g. `type(scope): desc [TICKET]`, given to the drafting model.
    pub title_format: Option<String>,
    /// Allowed title types; empty means no title check.
    #[serde(default)]
    pub types: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Critic {
    pub model: String,
    pub effort: String,
    /// Fix rounds a task gets, shared by gate failures and blocking findings.
    pub max_rounds: u32,
}

#[derive(Debug, Deserialize)]
pub struct Gate {
    pub steps: Vec<Step>,
}

/// `run` may use `{crates}` (`-p` flags) or `{paths}` (changed top-level dirs).
#[derive(Debug, Deserialize)]
pub struct Step {
    pub name: String,
    pub run: String,
    /// Regex; the step runs only when a changed file's path or content matches.
    pub when_files_contain: Option<String>,
    /// Globs; the step runs only when a changed path matches one.
    #[serde(default)]
    pub when_changed: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Worker {
    pub model: String,
    pub effort: String,
    /// The spend ceiling per task in USD, unless the task sets its own; 0 turns it off.
    pub budget_usd: f64,
    pub slots: u32,
    /// Workers running at once.
    pub concurrency: u32,
    /// Relative to the slot, e.g. `.mcp.json`.
    pub mcp_config: Option<String>,
    #[serde(default)]
    pub read_tools: Vec<String>,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Added to the built-in deny list.
    #[serde(default)]
    pub deny: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Watch {
    /// How long a silent stream and an idle process tree last before a nudge, e.g. `15m`.
    #[serde(deserialize_with = "duration")]
    pub stall_after: Duration,
    /// Nudges a task gets before a stall or loop fails it.
    pub nudges: u8,
    /// The same tool call this many times in a row is a loop.
    pub loop_repeats: u32,
    /// Context window in tokens, passed as `--autocompact`.
    pub autocompact: u64,
    /// Share of the window at which a worker hands off to a fresh session.
    pub handoff_at: f64,
    /// Fresh sessions a task gets after its first.
    pub max_handoffs: u32,
}

/// `90s`, `15m` or `1h`.
fn duration<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
    let s = String::deserialize(d)?;
    let (n, unit) = s.split_at(s.len().saturating_sub(1));
    let secs = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => 0,
    };
    match n.parse::<u64>() {
        Ok(n) if secs > 0 => Ok(Duration::from_secs(n * secs)),
        _ => Err(serde::de::Error::custom(format!(
            "bad duration {s:?}, expected e.g. 90s, 15m or 1h"
        ))),
    }
}

#[derive(Debug, Deserialize)]
pub struct Build {
    pub max_cargo: u32,
}

#[derive(Debug, Deserialize)]
pub struct Cargo {
    /// Runs in place of cargo for builds, e.g. `./scripts/cargo-worktree.sh`.
    pub wrapper: Option<String>,
    /// Open-file limit for everything a worker runs; launchd's default is 256.
    pub nofile: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct Role {
    pub model: String,
    pub effort: String,
}

#[derive(Debug, Deserialize)]
pub struct Ask {
    pub model: String,
    pub effort: String,
    /// Questions answering at once; they take no slot.
    pub concurrency: u32,
}

/// Slot `n` gets ports `base + n * per_slot` onwards.
#[derive(Debug, Deserialize)]
pub struct Ports {
    pub base: u32,
    pub per_slot: u32,
}

#[derive(Debug, Deserialize)]
pub struct Scripts {
    pub setup: Option<String>,
    pub teardown: Option<String>,
    pub run: Option<String>,
}

/// Defaults, then `~/.config/yogan/config.toml`, then the project file for `checkout`.
pub fn load(checkout: &Path) -> Result<Config> {
    load_in(checkout, &home()?)
}

fn load_in(checkout: &Path, home: &Path) -> Result<Config> {
    let (global, project) = tables(checkout, home)?;
    layered(global, project.map(|(_, t)| t))
}

pub fn home() -> Result<PathBuf> {
    std::env::home_dir().context("no home directory")
}

fn global_path(home: &Path) -> PathBuf {
    home.join(".config/yogan/config.toml")
}

/// The global file's table and, when `[projects]` maps `checkout`, its project file and table.
fn tables(checkout: &Path, home: &Path) -> Result<(Table, Option<(PathBuf, Table)>)> {
    let global = read_toml(&global_path(home))?.unwrap_or_default();
    let projects: BTreeMap<String, String> = match global.get("projects") {
        Some(p) => p.clone().try_into().context("[projects]")?,
        None => BTreeMap::new(),
    };
    let project = match find_project(&projects, origin_name(checkout).as_deref(), checkout, home) {
        Some(file) => {
            let path = expand(file, home);
            let table = read_toml(&path)?;
            let table =
                table.with_context(|| format!("project file {} not found", path.display()))?;
            Some((path, table))
        }
        None => None,
    };
    Ok((global, project))
}

/// The file Settings saves to, and the values in effect for it: the defaults and the global
/// file merged with, unless `global`, this checkout's project file.
pub fn settings(checkout: &Path, home: &Path, global: bool) -> Result<(PathBuf, Table)> {
    let (table, project) = tables(checkout, home)?;
    let (path, project) = match (global, project) {
        (true, _) => (global_path(home), None),
        (false, Some((path, project))) => (path, Some(project)),
        (false, None) => (new_project(checkout, home), None),
    };
    Ok((path, merged(table, project)))
}

/// Sets each `(table, key, value)` in the file Settings saves to, keeping the rest of it. A new
/// project file is also added to the global file's `[projects]`. Returns the file.
pub fn save(
    checkout: &Path,
    home: &Path,
    global: bool,
    values: &[(&str, &str, Value)],
) -> Result<PathBuf> {
    let (mut table, project) = tables(checkout, home)?;
    let (path, mut file) = match (global, project) {
        (true, _) => (global_path(home), table),
        (false, Some(project)) => project,
        (false, None) => {
            let path = new_project(checkout, home);
            let key = origin_name(checkout).unwrap_or_else(|| checkout.display().to_string());
            let projects = table.entry("projects").or_insert(Table::new().into());
            let projects = projects
                .as_table_mut()
                .context("[projects] is not a table")?;
            projects.insert(key, path.display().to_string().into());
            write(&global_path(home), &table)?;
            (path.clone(), read_toml(&path)?.unwrap_or_default())
        }
    };
    for (name, key, value) in values {
        let t = file.entry(*name).or_insert(Table::new().into());
        let t = t
            .as_table_mut()
            .with_context(|| format!("[{name}] is not a table"))?;
        t.insert(key.to_string(), value.clone());
    }
    write(&path, &file)?;
    Ok(path)
}

/// `~/.config/yogan/projects/<origin repo name, or checkout folder>.toml`.
fn new_project(checkout: &Path, home: &Path) -> PathBuf {
    let folder = checkout.file_name().unwrap_or_default().to_string_lossy();
    let name = origin_name(checkout).unwrap_or_else(|| folder.into_owned());
    home.join(format!(".config/yogan/projects/{name}.toml"))
}

fn write(path: &Path, table: &Table) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, toml::to_string(table)?).with_context(|| path.display().to_string())
}

fn layered(global: Table, project: Option<Table>) -> Result<Config> {
    Ok(merged(global, project).try_into()?)
}

fn merged(global: Table, project: Option<Table>) -> Table {
    let mut table: Table = DEFAULTS.parse().expect("defaults.toml parses");
    merge(&mut table, global);
    if let Some(project) = project {
        merge(&mut table, project);
    }
    table
}

fn merge(base: &mut Table, over: Table) {
    for (key, value) in over {
        match (base.get_mut(&key), value) {
            (Some(Value::Table(b)), Value::Table(o)) => merge(b, o),
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

fn read_toml(path: &Path) -> Result<Option<Table>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s.parse().with_context(|| path.display().to_string())?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| path.display().to_string()),
    }
}

/// A `[projects]` key is the origin's repo name or, failing that, the checkout path.
fn find_project<'a>(
    projects: &'a BTreeMap<String, String>,
    name: Option<&str>,
    checkout: &Path,
    home: &Path,
) -> Option<&'a String> {
    name.and_then(|n| projects.get(n)).or_else(|| {
        projects
            .iter()
            .find(|(key, _)| expand(key, home) == checkout)
            .map(|(_, file)| file)
    })
}

pub(crate) fn origin_name(checkout: &Path) -> Option<String> {
    let url = crate::git(checkout, &["remote", "get-url", "origin"]).ok()?;
    Some(repo_name(&url).to_string())
}

fn repo_name(url: &str) -> &str {
    let url = url.trim_end_matches('/');
    let name = url.rsplit(['/', ':']).next().unwrap_or(url);
    name.strip_suffix(".git").unwrap_or(name)
}

fn expand(path: &str, home: &Path) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(s: &str) -> Table {
        s.parse().unwrap()
    }

    #[test]
    fn precedence() {
        let global = table("[worker]\nmodel = \"global\"\neffort = \"low\"");
        let project = table("[worker]\nmodel = \"project\"\n[ports]\nbase = 0\nper_slot = 80");
        let cfg = layered(global, Some(project)).unwrap();
        // project beats global key by key; untouched keys keep the global value
        assert_eq!(cfg.worker.model, "project");
        assert_eq!(cfg.worker.effort, "low");
        // other roles keep defaults
        assert_eq!(cfg.lead.model, "claude-opus-5-5");
        assert_eq!(cfg.ports.map(|p| p.per_slot), Some(80));
        assert!(cfg.scripts.is_none());
        assert_eq!(cfg.build.max_cargo, 2);
        assert!(cfg.worker.allowed_tools.contains(&"Edit".to_string()));
        assert_eq!(cfg.watch.stall_after, Duration::from_secs(15 * 60));
        let bad = table("[watch]\nstall_after = \"15 min\"");
        assert!(layered(bad, None).is_err());
    }

    #[test]
    fn run_script() {
        let project = table("[scripts]\nrun = \"npm run dev\"");
        let cfg = layered(Table::new(), Some(project)).unwrap();
        assert_eq!(cfg.scripts.unwrap().run.as_deref(), Some("npm run dev"));
        let project = table("[scripts]\nsetup = \"make db\"");
        let cfg = layered(Table::new(), Some(project)).unwrap();
        assert!(cfg.scripts.unwrap().run.is_none());
    }

    #[test]
    fn settings_save_reloads_identically() {
        let root = std::env::temp_dir().join(format!("yogan-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (home, checkout) = (root.join("home"), root.join("work/trader"));
        std::fs::create_dir_all(&checkout).unwrap();
        let global = global_path(&home);
        write(&global, &table("[worker]\nmodel = \"g\"")).unwrap();

        // no project file yet: saving creates one and maps the checkout to it
        let (path, _) = settings(&checkout, &home, false).unwrap();
        assert_eq!(path, home.join(".config/yogan/projects/trader.toml"));
        let values = [
            ("worker", "effort", Value::from("max")),
            ("watch", "stall_after", Value::from("30m")),
            ("watch", "handoff_at", Value::from(0.85)),
        ];
        assert_eq!(save(&checkout, &home, false, &values).unwrap(), path);
        let cfg = load_in(&checkout, &home).unwrap();
        assert_eq!(
            (cfg.worker.model.as_str(), cfg.worker.effort.as_str()),
            ("g", "max")
        );
        assert_eq!(cfg.watch.stall_after, Duration::from_secs(30 * 60));
        assert_eq!(cfg.watch.handoff_at, 0.85);
        let (_, shown) = settings(&checkout, &home, false).unwrap();
        for (name, key, value) in &values {
            assert_eq!(shown[*name][*key], *value);
        }

        // hand-written tables survive a save, and the global file is a separate target
        let mut file = read_toml(&path).unwrap().unwrap();
        file.insert("scripts".into(), table("run = \"make dev\"").into());
        write(&path, &file).unwrap();
        save(&checkout, &home, false, &[("lead", "model", "p".into())]).unwrap();
        save(&checkout, &home, true, &[("lead", "model", "g2".into())]).unwrap();
        let cfg = load_in(&checkout, &home).unwrap();
        assert_eq!(cfg.lead.model, "p");
        assert_eq!(cfg.scripts.unwrap().run.as_deref(), Some("make dev"));
        let (_, global_view) = settings(&checkout, &home, true).unwrap();
        assert_eq!(global_view["lead"]["model"].as_str(), Some("g2"));
        assert_eq!(global_view["worker"]["effort"].as_str(), Some("high"));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn repo_names() {
        assert_eq!(repo_name("git@github.com:acme/fuse-os.git"), "fuse-os");
        assert_eq!(repo_name("https://github.com/acme/fuse-os"), "fuse-os");
        assert_eq!(repo_name("https://github.com/acme/fuse-os.git/"), "fuse-os");
        assert_eq!(repo_name("/srv/git/trader.git"), "trader");
    }

    #[test]
    fn project_lookup() {
        let home = Path::new("/home/u");
        let projects = BTreeMap::from([
            ("fuse-os".to_string(), "fuse.toml".to_string()),
            ("~/work/trader".to_string(), "trader.toml".to_string()),
        ]);
        let trader = Path::new("/home/u/work/trader");
        let find =
            |name, checkout| find_project(&projects, name, checkout, home).map(String::as_str);
        assert_eq!(find(Some("fuse-os"), Path::new("/x")), Some("fuse.toml"));
        assert_eq!(find(None, trader), Some("trader.toml"));
        assert_eq!(find(Some("unlisted"), trader), Some("trader.toml"));
        assert_eq!(find(Some("unlisted"), Path::new("/x")), None);
    }
}
