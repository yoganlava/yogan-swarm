use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
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
}
