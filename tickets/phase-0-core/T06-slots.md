- [ ] **T06 Slots**

`slots/1..N` persistent worktrees, `slots/<n>.lock` via fd-lock, branch `<branch_prefix><slug>` from `origin/main` (or parent's `origin/<branch>`), `git worktree prune` on setup.

Done when: test against a temp repo allocates, reuses and locks a slot.
