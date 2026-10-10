//! Starts ready tasks. The TUI calls it after changes and every worker calls it on exit, so
//! the queue keeps draining with the TUI closed.

use std::fs::{self, File};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;

use crate::task::{self, Status, Task};
use crate::{config, slot, worker};

/// Under `sched.lock`, starts ready tasks up to `[worker] concurrency` and the free slots.
pub fn run(repo: &Path) -> Result<()> {
    let state = task::state_dir(repo)?;
    let cfg = config::load(repo)?;
    fs::create_dir_all(&state)?;
    let lock = File::create(state.join("sched.lock"))?;
    lock.lock()?;
    let tasks = task::load_all(&state)?;
    slot::free_disk(&state, &tasks, cfg.worker.slots, cfg.disk.min_free_gb)?;
    if parked(&state, now()) {
        return Ok(());
    }
    for mut task in ready(&tasks, cfg.worker.concurrency, cfg.worker.slots) {
        task.status = Status::Running;
        task.pid = Some(worker::spawn(repo, &task.id, &[])?);
        // saved before the lock drops, so a concurrent call can't start it twice
        task.save(&state)?;
    }
    Ok(())
}

/// Holds new workers back until `until` (Unix seconds), for a rate limit.
pub(crate) fn park(state: &Path, until: u64) -> Result<()> {
    Ok(fs::write(state.join("rate_limit"), until.to_string())?)
}

fn parked(state: &Path, now: u64) -> bool {
    let until = fs::read_to_string(state.join("rate_limit")).unwrap_or_default();
    until.trim().parse().is_ok_and(|until: u64| now < until)
}

pub(crate) fn now() -> u64 {
    let since = SystemTime::now().duration_since(UNIX_EPOCH);
    since.map_or(0, |d| d.as_secs())
}

/// `Approved` tasks whose parent, if any, has its PR open or is merged, as many as the limits leave room for.
fn ready(tasks: &[Task], concurrency: u32, slots: u32) -> Vec<Task> {
    let running = |t: &Task| matches!(t.status, Status::Running | Status::Checking);
    let busy = tasks.iter().filter(|t| running(t)).count();
    let held = tasks
        .iter()
        .filter(|t| t.slot.is_some() || running(t))
        .count();
    let room = (concurrency as usize)
        .saturating_sub(busy)
        .min((slots as usize).saturating_sub(held));
    let parent_open = |t: &Task| match &t.parent {
        None => true,
        Some(p) => tasks
            .iter()
            .any(|o| &o.id == p && matches!(o.status, Status::PrOpen | Status::Merged)),
    };
    let mut ready: Vec<Task> = tasks
        .iter()
        .filter(|t| t.status == Status::Approved && parent_open(t))
        .cloned()
        .collect();
    // ponytail: oldest-first needs a created-at; id order until the lead's ids say otherwise
    ready.sort_by(|a, b| a.id.cmp(&b.id));
    ready.truncate(room);
    ready
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, status: Status) -> Task {
        Task {
            id: id.into(),
            status,
            ..Default::default()
        }
    }

    fn ids(tasks: Vec<Task>) -> Vec<String> {
        tasks.into_iter().map(|t| t.id).collect()
    }

    #[test]
    fn readiness_and_limits() {
        let mut child = task("c", Status::Approved);
        child.parent = Some("p".into());
        let mut parent = task("p", Status::Review);
        parent.slot = Some(1);
        let mut tasks = vec![
            task("b", Status::Approved),
            task("a", Status::Approved),
            task("x", Status::Proposed),
            child,
            parent,
        ];

        // the child waits for its parent's PR; others go in id order
        assert_eq!(ids(ready(&tasks, 4, 5)), ["a", "b"]);
        assert_eq!(ids(ready(&tasks, 1, 5)), ["a"]);
        tasks[4].status = Status::PrOpen;
        assert_eq!(ids(ready(&tasks, 4, 5)), ["a", "b", "c"]);
        tasks[4].status = Status::Merged;
        assert_eq!(ids(ready(&tasks, 4, 5)), ["a", "b", "c"]);
        tasks[4].status = Status::PrOpen;

        // running and checking tasks use up concurrency, and their slots
        tasks.push(task("r", Status::Running));
        tasks.push(task("k", Status::Checking));
        assert_eq!(ids(ready(&tasks, 3, 5)), ["a"]);
        // a task in Review still holds its slot: 5 slots - p, r, k = 2
        assert_eq!(ids(ready(&tasks, 9, 5)), ["a", "b"]);
        assert!(ready(&tasks, 2, 5).is_empty());
    }

    #[test]
    fn a_rate_limit_holds_new_workers_until_its_reset() {
        let state = std::env::temp_dir().join(format!("yogan-sched-{}", std::process::id()));
        fs::create_dir_all(&state).unwrap();
        assert!(!parked(&state, 100));
        park(&state, 200).unwrap();
        assert!(parked(&state, 100));
        assert!(!parked(&state, 200));
        fs::remove_dir_all(&state).unwrap();
    }
}
