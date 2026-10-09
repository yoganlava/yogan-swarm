use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, ensure};
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
    /// Writes `tasks/<id>.toml` atomically: a crash leaves the old file or the new one.
    pub fn save(&self, dir: &Path) -> Result<()> {
        let tasks = dir.join("tasks");
        fs::create_dir_all(&tasks)?;
        let path = tasks.join(format!("{}.toml", self.id));
        let tmp = tasks.join(format!("{}.{}.tmp", self.id, std::process::id()));
        let mut file = fs::File::create(&tmp)?;
        file.write_all(toml::to_string(self)?.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, &path).with_context(|| path.display().to_string())
    }
}

pub fn load_all(dir: &Path) -> Result<Vec<Task>> {
    let entries = match fs::read_dir(dir.join("tasks")) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        entries => entries?,
    };
    let mut tasks = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "toml") {
            let text = fs::read_to_string(&path)?;
            tasks.push(toml::from_str(&text).with_context(|| path.display().to_string())?);
        }
    }
    Ok(tasks)
}

/// `t<unix seconds>`, so ids sort by creation.
pub fn now_id() -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("t{secs}")
}

/// An approved task with a free id and branch `<prefix><slug>`.
pub fn new_task(
    title: &str,
    body: &str,
    ticket: &str,
    prefix: &str,
    tasks: &[Task],
    id: String,
) -> Result<Task> {
    ensure!(!title.trim().is_empty(), "write a request first");
    let taken = |s: &str| tasks.iter().any(|t| t.id == s);
    let mut id = id;
    while taken(&id) {
        id.push('a'); // two submits in one second
    }
    let slug = slug(title);
    let branch_taken = |b: &str| tasks.iter().any(|t| t.branch == b);
    let mut branch = format!("{prefix}{slug}");
    for n in 2.. {
        if !branch_taken(&branch) {
            break;
        }
        branch = format!("{prefix}{slug}-{n}");
    }
    Ok(Task {
        id,
        title: title.trim().into(),
        body: body.trim().into(),
        ticket: (!ticket.is_empty()).then(|| ticket.into()),
        status: Status::Approved,
        branch,
        ..Default::default()
    })
}

/// Files a lead proposal as `Proposed` and returns its id.
pub fn propose(
    dir: &Path,
    prefix: &str,
    title: &str,
    body: &str,
    parent: Option<String>,
    crates: Vec<String>,
    accept: Vec<String>,
) -> Result<String> {
    ensure!(
        (1..=5).contains(&accept.len()),
        "give 1 to 5 --accept criteria, got {}",
        accept.len()
    );
    ensure!(
        accept.iter().all(|a| !a.trim().is_empty()),
        "an --accept criterion is empty"
    );
    let tasks = load_all(dir)?;
    if let Some(p) = &parent {
        ensure!(
            tasks.iter().any(|t| &t.id == p),
            "no task {p} to use as --parent"
        );
    }
    let task = Task {
        status: Status::Proposed,
        acceptance: accept.iter().map(|a| a.trim().into()).collect(),
        parent,
        crates,
        ..new_task(title, body, "", prefix, &tasks, now_id())?
    };
    task.save(dir)?;
    Ok(task.id)
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
    fn composed_task() {
        let other = Task {
            id: "t5".into(),
            branch: "u/reject-negative-max-delay".into(),
            ..Default::default()
        };
        let t = new_task(
            "Reject negative max_delay!",
            "It panics later.\n",
            "CC-687",
            "u/",
            &[other],
            "t5".into(),
        )
        .unwrap();
        assert_eq!(
            (t.id.as_str(), t.title.as_str()),
            ("t5a", "Reject negative max_delay!")
        );
        assert_eq!(
            (t.body.as_str(), t.ticket.as_deref()),
            ("It panics later.", Some("CC-687"))
        );
        assert_eq!(
            (t.status, t.branch.as_str()),
            (Status::Approved, "u/reject-negative-max-delay-2")
        );
        assert!(new_task(" ", "body", "", "", &[], "t6".into()).is_err());
        assert_eq!(slug("Ünïcode & ...!"), "n-code");
        assert_eq!(
            slug("Split the ledger reconciliation job into per-account batches"),
            "split-the-ledger-reconciliation-job-into"
        );
    }

    #[test]
    fn propose_needs_one_to_five_criteria() {
        let dir = std::env::temp_dir().join(format!("yogan-propose-{}", std::process::id()));
        let accept = |n: usize| (0..n).map(|i| format!("criterion {i}")).collect();
        let propose = |accept, parent| {
            propose(
                &dir,
                "u/",
                "Reject negative",
                "b",
                parent,
                vec!["ledger".into()],
                accept,
            )
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
        assert_eq!(tasks.len(), 2);

        fs::remove_dir_all(&dir).unwrap();
    }
}
