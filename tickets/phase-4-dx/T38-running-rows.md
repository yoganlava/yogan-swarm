- [ ] **T38 Running rows show the step and idle time**

Plan: [yogan TUI north star](https://claude.ai/artifact/Gfb7mbk3hAoYv9khzXBtk9) › A · Main screen; problem 2.

A second, dim line under each Running task: the latest tool call (verb + target, as in Activity) and how long the stream has been quiet. Amber `quiet Nm` once quiet for more than half of `[watch] stall_after`.

Done when: snapshot of a running row with a step, and one past the quiet threshold.

## Current code

- `list` builds `working · {age}` with a `ponytail` note (`src/tui.rs:1563`).
- `activity` (~L1168) parses `logs/<id>.jsonl`; idle time is `now` minus that file's mtime. Read only a small tail per running task, since this runs on every redraw.
