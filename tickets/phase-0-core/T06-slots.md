- [x] **T06 Slots**

Plan: Workers ("A new task checks out branch…"); Task lifecycle › Scheduling rules (branch base); Workers › Disk and cargo clean (worktree prune); Open questions and assumptions › Assumptions (why not `--worktree`).

`slots/1..N` persistent worktrees, `slots/<n>.lock` via std `File::try_lock` (flock), task's branch checked out fresh from a given base (`origin/main`; parent base is T23), `git worktree prune` on setup. A slot is taken while a task's `slot` names it.

Done when: test against a temp repo allocates, reuses and locks a slot.
