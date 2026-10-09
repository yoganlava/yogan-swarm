- [x] **T12 Gate**

Plan: Gate, review and PRs (gate steps); Workers › Slot scripts and ports (`[gate] steps`, `when_files_contain`); Task lifecycle (`max_rounds`).

Clean tree + new commits; changed files → crates via `cargo metadata --no-deps` (`{paths}` otherwise); `[gate]` steps with `when_files_contain`; `logs/<id>.gate.log`. Fail → resume worker, up to `max_rounds`; then `Review`. Adds `Task.gate`.

Done when: tests for crate mapping and conditional steps.
