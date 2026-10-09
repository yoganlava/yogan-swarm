- [x] **T09 Cargo shim + build permits**

Plan: Workers ("Machine-wide build throttle", `CARGO_BUILD_JOBS`); Open questions and assumptions (flock release).

Shim = `<state>/bin/cargo` symlinked to yogan (argv[0] dispatch), first on slot PATH; `max_cargo` machine-wide flock permits in `~/.local/share/yogan/permits/`; shared flock on `slots/<n>.build`; `cargo test` → `--no-run` under permit, binaries outside; read-only subcommands skip the queue; the shim strips itself from PATH for everything it runs, which replaces `rustup which cargo` and `YOGAN_IN_SHIM`; optional `[cargo] wrapper`; `CARGO_BUILD_JOBS = cores / max_cargo`.

Done when: test shows `max_cargo + 1` concurrent builds queue one.
