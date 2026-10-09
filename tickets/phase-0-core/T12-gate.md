- [ ] **T12 Gate**

Clean tree + new commits; changed files → crates via `cargo metadata --no-deps` (`{paths}` otherwise); `[gate]` steps with `when_files_contain`; `logs/<id>.gate.log`. Fail → resume worker, up to `max_rounds`; then `Review`.

Done when: tests for crate mapping and conditional steps.
