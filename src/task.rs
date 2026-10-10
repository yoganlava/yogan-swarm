use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, ensure};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Proposed,
    Approved,
    Running,
    Checking,
    Review,
    PrOpen,
    Failed,
    Discarded,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub plan: String,
    pub title: String,
    pub body: String,
    pub ticket: Option<String>,
    pub acceptance: Vec<String>,
    pub parent: Option<String>,
    pub crates: Vec<String>,
    pub status: Status,
    pub slot: Option<u32>,
    pub pid: Option<u32>,
    pub sessions: Vec<String>,
    pub branch: String,
    pub nudges: u8,
    /// The latest `total_cost_usd` of each session, which is cumulative within a session.
    #[serde(default)]
    pub usage: BTreeMap<String, f64>,
    /// The spend ceiling in USD; 0 turns it off.
    pub budget_usd: Option<f64>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub summary: Option<String>,
    pub gate: Option<Vec<crate::gate::Check>>,
    /// Title and body awaiting approval.
    pub pr_draft: Option<crate::pr::Draft>,
    pub pr_url: Option<String>,
}

/// `$YOGAN_DIR`, else `~/.local/share/yogan/<repo>/`.
pub fn state_dir(checkout: &Path) -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("YOGAN_DIR") {
        return Ok(dir.into());
    }
    let home = std::env::home_dir().context("no home directory")?;
    let repo = crate::config::origin_name(checkout)
        .or_else(|| Some(checkout.file_name()?.to_str()?.to_string()))
        .context("no repo name for the checkout")?;
    Ok(home.join(".local/share/yogan").join(repo))
}

impl Task {
    /// USD spent across the task's sessions.
    pub fn spent(&self) -> f64 {
        // from 0.0, since an empty f64 `sum` is -0.0, which formats as `-0.00`
        self.usage.values().fold(0.0, |a, b| a + b)
    }

    /// Writes `tasks/<id>.toml` atomically.
    pub fn save(&self, dir: &Path) -> Result<()> {
        write_toml(&dir.join("tasks"), &self.id, self)
    }
}

/// Writes `<dir>/<id>.toml` atomically: a crash leaves the old file or the new one.
pub(crate) fn write_toml(dir: &Path, id: &str, value: &impl Serialize) -> Result<()> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{id}.toml"));
    let tmp = dir.join(format!("{id}.{}.tmp", std::process::id()));
    let mut file = fs::File::create(&tmp)?;
    file.write_all(toml::to_string(value)?.as_bytes())?;
    file.sync_all()?;
    fs::rename(&tmp, &path).with_context(|| path.display().to_string())
}

pub fn load_all(dir: &Path) -> Result<Vec<Task>> {
    read_toml_dir(&dir.join("tasks"))
}

/// Every `<dir>/*.toml`; a missing `dir` has none.
pub(crate) fn read_toml_dir<T: DeserializeOwned>(dir: &Path) -> Result<Vec<T>> {
    let entries = match fs::read_dir(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        entries => entries?,
    };
    let mut values = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "toml") {
            let text = fs::read_to_string(&path)?;
            values.push(toml::from_str(&text).with_context(|| path.display().to_string())?);
        }
    }
    Ok(values)
}

/// What `t` branches from, rebases onto and targets with its PR: its parent's pushed branch,
/// else `origin/main`.
pub fn base<'a>(t: &Task, mut tasks: impl Iterator<Item = &'a Task>) -> String {
    let parent = t.parent.as_ref().and_then(|p| tasks.find(|o| &o.id == p));
    parent.map_or("origin/main".into(), |p| format!("origin/{}", p.branch))
}

/// `<prefix><unix seconds>`, so ids sort by creation.
pub fn now_id(prefix: &str) -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{prefix}{secs}")
}

/// Files a lead proposal as `Proposed` with a free id and branch `<prefix><slug>`, and returns
/// its id. Holds `tasks.lock` meanwhile, so concurrent filers can't pick the same ones.
pub fn propose(dir: &Path, prefix: &str, proposal: Task) -> Result<String> {
    fs::create_dir_all(dir)?;
    let lock = fs::File::create(dir.join("tasks.lock"))?;
    lock.lock()?;
    let tasks = load_all(dir)?;
    check_proposal(&proposal, &tasks)?;
    let mut id = now_id("t");
    while tasks.iter().any(|t| t.id == id) {
        id.push('a'); // two proposals in one second
    }
    let slug = slug(&proposal.title);
    let mut branch = format!("{prefix}{slug}");
    for n in 2.. {
        if !tasks.iter().any(|t| t.branch == branch) {
            break;
        }
        branch = format!("{prefix}{slug}-{n}");
    }
    let task = Task {
        id,
        branch,
        status: Status::Proposed,
        title: proposal.title.trim().into(),
        body: proposal.body.trim().into(),
        acceptance: proposal
            .acceptance
            .iter()
            .map(|a| a.trim().into())
            .collect(),
        ..proposal
    };
    task.save(dir)?;
    Ok(task.id)
}

/// What filing or editing a proposal requires: a title, 1 to 5 criteria and a known parent.
pub fn check_proposal(t: &Task, tasks: &[Task]) -> Result<()> {
    ensure!(!t.title.trim().is_empty(), "the title is empty");
    let n = t.acceptance.len();
    ensure!(
        (1..=5).contains(&n),
        "give 1 to 5 acceptance criteria, got {n}"
    );
    ensure!(
        t.acceptance.iter().all(|a| !a.trim().is_empty()),
        "an acceptance criterion is empty"
    );
    if let Some(p) = &t.parent {
        ensure!(
            p != &t.id && tasks.iter().any(|o| &o.id == p),
            "no task {p} to use as the parent"
        );
    }
    Ok(())
}

/// `Reject negative max_delay!` → `reject-negative-max-delay`, at most 40 chars.
fn slug(title: &str) -> String {
    let lower = title.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let slug = words.join("-"); // ASCII only, so any byte cut is a char boundary
    let slug = slug[..slug.len().min(40)].trim_end_matches('-');
    if slug.is_empty() {
        "task".into()
    } else {
        slug.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_crash_mid_write() {
        let dir = std::env::temp_dir().join(format!("yogan-task-{}", std::process::id()));
        let mut task = Task {
            id: "t1".into(),
            plan: "p1".into(),
            title: "Reject negative max_delay".into(),
            body: "body".into(),
            ticket: Some("CC-687".into()),
            acceptance: vec!["a negative max_delay is rejected at parse time".into()],
            parent: None,
            crates: vec!["ledger".into()],
            status: Status::PrOpen,
            slot: Some(1),
            pid: None,
            sessions: vec![],
            branch: "u/reject-negative".into(),
            nudges: 0,
            usage: BTreeMap::from([("s-1".into(), 1.25)]),
            budget_usd: Some(5.0),
            model: None,
            effort: Some("high".into()),
            summary: None,
            gate: None,
            pr_draft: None,
            pr_url: None,
        };
        task.save(&dir).unwrap();
        task.status = Status::Review;
        task.save(&dir).unwrap();
        assert_eq!(load_all(&dir).unwrap(), vec![task.clone()]);

        // a writer that died mid-write leaves only a partial tmp file
        fs::write(dir.join("tasks/t1.999.tmp"), "id = \"t1\"\nstat").unwrap();
        assert_eq!(load_all(&dir).unwrap(), vec![task]);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn base_is_the_parents_branch() {
        let parent = Task {
            id: "t1".into(),
            branch: "u/parent".into(),
            ..Default::default()
        };
        let child = Task {
            id: "t2".into(),
            parent: Some("t1".into()),
            ..Default::default()
        };
        let tasks = [parent.clone(), child.clone()];
        assert_eq!(base(&child, tasks.iter()), "origin/u/parent");
        assert_eq!(base(&parent, tasks.iter()), "origin/main");
    }

    #[test]
    fn propose_needs_one_to_five_criteria() {
        let dir = std::env::temp_dir().join(format!("yogan-propose-{}", std::process::id()));
        let accept = |n: usize| (0..n).map(|i| format!("criterion {i}")).collect();
        let propose = |acceptance, parent| {
            let proposal = Task {
                title: "Reject negative".into(),
                body: "b".into(),
                plan: "r1".into(),
                ticket: Some("CC-9".into()),
                crates: vec!["ledger".into()],
                acceptance,
                parent,
                ..Default::default()
            };
            propose(&dir, "u/", proposal)
        };
        assert!(propose(accept(0), None).is_err());
        assert!(propose(accept(6), None).is_err());
        assert!(propose(vec![" ".into()], None).is_err());
        assert!(propose(accept(1), Some("nope".into())).is_err());

        let id = propose(accept(5), None).unwrap();
        let child = propose(accept(1), Some(id.clone())).unwrap();
        let tasks = load_all(&dir).unwrap();
        let t = tasks.iter().find(|t| t.id == child).unwrap();
        assert_eq!(t.status, Status::Proposed);
        assert_eq!(t.parent.as_deref(), Some(id.as_str()));
        assert_eq!(t.branch, "u/reject-negative-2");
        assert_eq!(t.acceptance, vec!["criterion 0"]);
        assert_eq!((t.plan.as_str(), t.ticket.as_deref()), ("r1", Some("CC-9")));
        assert_eq!(tasks.len(), 2);
        assert_ne!(id, child, "two proposals in one second get different ids");
        assert_eq!(slug("Ünïcode & ...!"), "n-code");
        assert_eq!(
            slug("Split the ledger reconciliation job into per-account batches"),
            "split-the-ledger-reconciliation-job-into"
        );

        fs::remove_dir_all(&dir).unwrap();
    }
}
