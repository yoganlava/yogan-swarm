- [ ] **T35b Permission modes and allowing denied calls**

`[worker] permission_mode` is passed as `--permission-mode` (default `acceptEdits`; `auto` lets Claude Code's classifier approve calls outside `allowed_tools`). Each denied call becomes an allow rule in `task.denied` (`Bash(<cmd>)`, `Read(//<path>)`, `Edit(//<path>)`, else the tool name). In Review, `a  allow denied` moves them to `task.allowed`, which the worker adds to `--allowedTools`, and resumes the session to retry. The built-in deny list still wins.

Done when: a worker run with `permission_mode = "auto"` against real Claude Code completes; a denied call shows `a  allow denied` in the TUI, and pressing it resumes the worker with the call allowed.
