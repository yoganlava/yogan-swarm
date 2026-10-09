use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::Deserialize;
use toml::{Table, Value};

const DEFAULTS: &str = include_str!("defaults.toml");

#[derive(Debug, Deserialize)]
pub struct Config {
    pub lead: Role,
    pub ask: Role,
    pub pr: Role,
    pub critic: Role,
    pub worker: Role,
}

#[derive(Debug, Deserialize)]
pub struct Role {
    pub model: String,
    pub effort: String,
}

/// Defaults, then `~/.config/yogan/config.toml`, then the project file for `checkout`.
pub fn load(checkout: &Path) -> Result<Config> {
    let home = std::env::home_dir().context("no home directory")?;
    let global = read_toml(&home.join(".config/yogan/config.toml"))?.unwrap_or_default();
    let projects: BTreeMap<String, String> = match global.get("projects") {
        Some(p) => p.clone().try_into().context("[projects]")?,
        None => BTreeMap::new(),
    };
    let project = match find_project(&projects, origin_name(checkout).as_deref(), checkout, &home) {
        Some(file) => {
            let path = expand(file, &home);
            let table = read_toml(&path)?;
            Some(table.with_context(|| format!("project file {} not found", path.display()))?)
        }
        None => None,
    };
    layered(global, project)
}

fn layered(global: Table, project: Option<Table>) -> Result<Config> {
    let mut table: Table = DEFAULTS.parse().expect("defaults.toml parses");
    merge(&mut table, global);
    if let Some(project) = project {
        merge(&mut table, project);
    }
    Ok(table.try_into()?)
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

fn origin_name(checkout: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    let url = String::from_utf8(out.stdout).ok()?;
    out.status
        .success()
        .then(|| repo_name(url.trim()).to_string())
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
        let project = table("[worker]\nmodel = \"project\"");
        let cfg = layered(global, Some(project)).unwrap();
        // project beats global key by key; untouched keys keep the global value
        assert_eq!(cfg.worker.model, "project");
        assert_eq!(cfg.worker.effort, "low");
        // other roles keep defaults
        assert_eq!(cfg.lead.model, "claude-opus-5-5");
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
