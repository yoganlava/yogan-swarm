- [x] **T32 Disk cleanup**

Plan: Workers › Disk and cargo clean.

Toolchain change, `min_free_gb`, `max_target_gb` divergence; takes slot lock + permit exclusively; never a running slot.

Done when: test that a running slot is skipped.

## From the plan (Workers › Disk and cargo clean)

*Persistent target directories keep builds warm, and they are also what fills the disk. yogan cleans them in tiers, cheapest first, and never touches a slot with a running task.*

| Trigger | Action |
|---|---|
| The Rust toolchain changes | `cargo clean` the slot, since artifacts from another toolchain are never reused |
| Free disk drops below `min_free_gb` | `cargo clean` idle slots, largest first, until back above the threshold |
| A task leaves a slot that has diverged from its seed by more than `max_target_gb` | Delete the slot's target dir and seed it again from main |

- *Cleaning takes the slot's lock and the slot's build permit exclusively, so the scheduler can't start a task there mid-clean and any build in the slot finishes first.*
- *A new or cleaned slot is re-seeded as described under Workers. `du` reports each clone at full size (five slots would read as about 595 GB on this machine), so slot size is measured as physical divergence from the seed, not `du`.*
- *Slot setup also runs `git worktree prune`, so stale worktrees from earlier runs don't pile up.*

Seeding, from Workers: *yogan seeds a new slot itself. It clones the main checkout's target dir without `debug/incremental` … It then copies each tracked file's mtime from the main checkout onto the slot's checkout (`touch -r`).*

Config, global defaults:

```toml
[disk]            # slot cleanup
max_target_gb    = 60    # per slot, measured as divergence from the seed
min_free_gb      = 100
```

## Current code

- `[disk]` is already in `src/defaults.toml`, but `config::Config` (`src/config.rs`) has no `Disk` struct yet.
- Slots, in `src/slot.rs`:
  - `claim` and `lock` take `slots/<n>.lock`; a running worker holds it for its whole life.
  - `prepare` checks out the slot and calls `seed`.
  - `seed` and `clone_tree` clone the main target dir without incremental, and `copy_mtimes` copies mtimes.
  - Slot dirs are `<state>/slots/<n>`, and each slot's target is `<slot>/target` (`CARGO_TARGET_DIR`).
- Build permit: every cargo build in slot `n` holds a **shared** flock on `<state>/slots/<n>.build` (`YOGAN_BUILD_LOCK`, `shim::run` in `src/shim.rs`). Cleaning takes it **exclusively** (`File::lock()`), which waits for running builds.
- A slot is left (freed) by `App::free_slot` in `src/tui.rs` (~L764), on PR open and on discard. `sched::run` (`src/sched.rs`) starts tasks into free slots under `sched.lock`.
- "Running" = `Status::Running | Status::Checking` (`tui::running`, and `sched::ready` counts them). A task in Review or Failed still holds `slot = Some(n)` but isn't running.
- The tree-walk and `sysinfo` helpers in `src/worker.rs` show how this repo reads process info, if needed.

## Not in the plan (decide, keep it small)

- How to detect a toolchain change: e.g. record `rustc -vV` per slot in `<state>/slots/<n>.toolchain` and compare before a task starts.
- How to measure "divergence from the seed": e.g. the blocks of files in the slot's target that are newer than the seed time, or sizes of files not shared by clone. Mark the heuristic with a `ponytail:` comment.
- Where cleanup runs: on slot free (divergence tier) and in `sched::run` before starting tasks (toolchain and free-disk tiers) covers all three.
