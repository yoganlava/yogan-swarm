- [ ] **T32 Disk cleanup**

Plan: Workers › Disk and cargo clean.

Toolchain change, `min_free_gb`, `max_target_gb` divergence; takes slot lock + permit exclusively; never a running slot.

Done when: test that a running slot is skipped.
