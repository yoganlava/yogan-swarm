- [ ] **T06 Slots**

Plan: Workers ("A new task checks out branch…"); Task lifecycle › Scheduling rules (branch base); Workers › Disk and cargo clean (worktree prune); Open questions and assumptions › Assumptions (why not `--worktree`).

`slots/1..N` persistent worktrees, `slots/<n>.lock` via fd-lock, branch `<branch_prefix><slug>` from `origin/main` (or parent's `origin/<branch>`), `git worktree prune` on setup.

Done when: test against a temp repo allocates, reuses and locks a slot.
