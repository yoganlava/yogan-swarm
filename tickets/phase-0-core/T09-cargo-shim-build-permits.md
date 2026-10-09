- [ ] **T09 Cargo shim + build permits**

Shim first on slot PATH; `max_cargo` flock permits (shared flock on `slots/<n>.build`); `cargo test` → `--no-run` under permit, binaries outside; `YOGAN_IN_SHIM=1` passthrough; `metadata`/`tree` skip queue; real cargo resolved once via `rustup which cargo`; `CARGO_BUILD_JOBS = cores / max_cargo`.

Done when: test shows `max_cargo + 1` concurrent builds queue one.
