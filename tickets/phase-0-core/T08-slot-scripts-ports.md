- [ ] **T08 Slot scripts + ports**

Plan: Workers › Slot scripts and ports (env var table and bullets).

`[scripts]` setup/teardown per task, `YOGAN_ROOT/SLOT/SLOT_DIR/PORT_BASE/PORT_COUNT/TASK_ID/CRATES` env, copy `.mcp.json` and `graft/` from main. Failed setup → `Failed` with log, before any Claude time. No `[scripts]` → plain worktree.

Done when: test with a dummy setup script, passing and failing.
