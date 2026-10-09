- [ ] **T03 State + task model**

Plan: Architecture (state directory layout); Task lifecycle (state diagram, `Task` struct, `Status` enum); Open questions and assumptions › Assumptions (state location).

`~/.local/share/yogan/<repo>/` layout (`YOGAN_DIR` overrides), `Task` struct and 8-state `Status`, atomic write (tmp + rename), load all of `tasks/`.

Done when: round-trip test; a crash mid-write leaves the old file intact.
