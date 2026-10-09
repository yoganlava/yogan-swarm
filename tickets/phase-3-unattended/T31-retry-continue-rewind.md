- [ ] **T31 Retry, continue, rewind**

Plan: Unattended safety (Failed task row); Gate, review and PRs (`w` paragraph); TUI › Keys (`c`, `w`).

Retry on chosen model; `c` runs `claude --resume` in slot; `w` rewinds to a worker commit with reason.

Done when: manual check of each key.

## From the plan

- Unattended safety table, "Failed task" row: *Retry it on the model you pick in the retry prompt.*
- Keys table:
  - `c`: *Continue interactively: suspend the TUI, run `claude --resume <id>` in the slot.* Applies to Review and Failed.
  - `w`: *Rewind the slot to one of the worker's commits and resume the session with your reason.* Applies to Review.
  - *`p`, `c`, `g` and `r` are scoped to the screen they are listed for.*
- Gate, review and PRs: *A task holds its slot until its PR opens, so feedback and fix rounds resume the original session in the same worktree. Claude Code will resume a session from any directory, so yogan always sets the working directory to the slot itself.*
- Screen 5 (Review with a failed gate): *`r` sends feedback and `c` drops you into the session.*
- Precedence: a task's own `model`/`effort` fields beat config; the resolved values are written to the task file at launch and shown in the header (`opus/high`).

## Current code

- Keys: `App::key` in `src/tui.rs` (~L519-689). Footer and help keys come from `KEYS` (~L45). `t` is already "retry" for a **failed request** (`App::retry`, ~L755), and `x` is discard.
- Suspending the TUI: the main loop in `tui::run` (~L270-315) already does it for `d` (`app.pager`: `ratatui::restore()`, run the command, `ratatui::init()`) and for `e` (`app.edit`). Copy that pattern for `c`.
- Resuming a task's session from the TUI: `act_on_finding` (~L882-913) calls `worker::spawn(&repo, &id, &["--reply", &prompt])`. `yogan worker <id> --reply <text>` (`src/main.rs`, `worker::run`) resumes `task.sessions.last()` in the task's existing slot, then reruns the gate and critic.
- A failed worker leaves `status = Failed`, keeps `slot` and `sessions`, and puts the error in `summary` (`worker::run`, `src/worker.rs`). `reap` (~L420) marks dead workers `Failed`.
- `Task` fields used here: `model`, `effort`, `sessions`, `slot`, `nudges` (stall and loop nudges used so far; `[watch] nudges` caps them per task), `usage`, `budget_usd` (`src/task.rs`).
- The worker's commits are `git log <base>..HEAD` in the slot. The base is `task::base(task, tasks)`, available in the TUI as `App::base()`.

## Not in the plan (decide, keep it small)

- Which key retries a failed task: reusing `t` (already labelled "retry") fits.
- Whether a retry resumes the old session in its slot (keeps its commits) or starts fresh. Resuming via `--reply` with the chosen model reuses the most code.
- Whether a retry resets `nudges`. Without a reset, a task failed by a stall fails again on its first stall.
- How `w` lists commits to pick from: a small list modal, like the findings cursor, would do.
