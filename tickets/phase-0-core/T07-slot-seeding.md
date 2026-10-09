- [ ] **T07 Slot seeding (Cargo repos only)**

Plan: Workers ("yogan seeds a new slot itself"); Workers › Disk and cargo clean (re-seeding, divergence).

Clone main target dir minus `debug/incremental`, then copy tracked files' mtimes (`touch -r`). Per-slot `CARGO_TARGET_DIR`.

Done when: on fuse-os, a fresh slot's test build rebuilds no workspace crates.
