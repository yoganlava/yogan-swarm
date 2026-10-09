- [ ] **T33 Run tab**

Plan: Workers › Slot scripts and ports (`R` bullet); TUI › Screens › 7. Run tab.

`R` starts/stops slot run script; output in Run tab; port range in header.

Done when: snapshot of Run tab.

## From the plan

- Slot scripts and ports: *`R` starts or stops the run script for the selected task; its output appears in a Run tab, so you can try a change in `Review` before opening the PR.* Scripts are optional: *With no `[scripts]` in the project file, slots are plain worktrees.*
- Screen 7, Run tab: *`R` on a task in Review starts its slot's run script. The tab shows each slot script's status, the slot's port range in the header, and the run output, so you can try the change before opening the PR. A running service is marked in the task list.*
- Keys table: `R`: *Start or stop the slot's run script; output in a Run tab.* Applies to Review.
- Widgets: detail tabs are *Summary, Activity, Gate, Findings, Diff, Run*, and `1–6` switch tabs.
- Scripts run in the slot with these variables, which Claude, the gate and the critic inherit:

| Variable | Meaning |
|---|---|
| `YOGAN_ROOT` | Your main checkout |
| `YOGAN_SLOT`, `YOGAN_SLOT_DIR` | Slot number and worktree path |
| `YOGAN_PORT_BASE`, `YOGAN_PORT_COUNT` | This slot's port range |
| `YOGAN_TASK_ID`, `YOGAN_CRATES` | The current task and its crates |

- Project config example: `[scripts] setup = "…"`, `teardown = "…"`. Ports come from `[ports] base`, `per_slot`; slot `n` gets `base + n * per_slot` onwards.

## Current code

- `config::Scripts` (`src/config.rs`) has `setup` and `teardown` only. The plan doesn't name the run script's key; `run` fits (`[scripts] run = "…"`).
- `slot::env` (`src/slot.rs`) builds the variables above; `slot::run_script` runs a script with a log file. `App::free_slot` (`src/tui.rs` ~L764) shows how the TUI runs teardown with `slot::env`.
- Tabs: `TABS` (`src/tui.rs` ~L42) has 5 entries, and `KeyCode::Char(c @ '1'..='5')` in `App::key` switches them. `App::load_tab` (~L700) loads per-tab data. `detail` (~L1426) dispatches per-tab draw functions (`activity_tab`, `gate_tab`, `diff_tab`), and `KEYS` (~L45) holds the footer and help keys.
- Snapshot tests: `tab_screen(app, tab)` and the `summary_tab` and `gate_tab_shows_failure` tests in `src/tui.rs` show the TestBackend snapshot pattern.
- Starting the script with `worker::detach` (`src/worker.rs`) gives a pid that is also its process group, so `worker::stop(pid)` stops it and everything it started.

## Not in the plan (decide, keep it small)

- Where the run's pid and log live, e.g. `<state>/slots/<n>.run.pid` and `logs/<id>.run.log`, so the tab survives a TUI restart.
- The list marker for a running service.
- Whether discard and PR open also stop a running service (they free the slot, so they probably should).
