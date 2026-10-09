- [ ] **T07 Slot seeding (Cargo repos only)**

Clone main target dir minus `debug/incremental`, then copy tracked files' mtimes (`touch -r`). Per-slot `CARGO_TARGET_DIR`.

Done when: on fuse-os, a fresh slot's test build rebuilds no workspace crates.
