- [ ] **T41 Running rows show the step and idle time**

Plan: [yogan TUI north star v2](https://claude.ai/artifact/PcPzsUATxe58br5igsDb3Z) › Try it (Working group).

A second, dim line under each Running task: the latest tool call (verb + target, as in Activity) and how long the stream has been quiet. Amber `quiet Nm` once quiet for more than half of `[watch] stall_after`.

Done when: snapshot of a running row with a step, and one past the quiet threshold.

## Current code

- `list` builds the row with a `ponytail` note on the step (`src/tui.rs:2181`).
- `activity` (~L1683) parses `logs/<id>.jsonl` (last 256 KiB); idle time is `now` minus that file's mtime.
