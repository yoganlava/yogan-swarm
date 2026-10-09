- [x] **T07 Slot seeding (Cargo repos only)**

Plan: Workers ("yogan seeds a new slot itself"); Workers › Disk and cargo clean (re-seeding, divergence).

On a slot's first checkout: CoW-clone the main target dir (from `cargo metadata`) into `<slot>/target` minus `incremental` dirs, then copy mtimes only for tracked files whose blob matches and that are unmodified in the main checkout (otherwise Cargo would skip a real change). Automated check on a tiny crate in `slot::tests`.

Done when: on fuse-os, a fresh slot's test build rebuilds no workspace crates.
