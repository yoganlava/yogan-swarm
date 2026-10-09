- [ ] **T36 The footer fits**

Plan: [yogan TUI north star](https://claude.ai/artifact/Gfb7mbk3hAoYv9khzXBtk9) › Keys; problem 1.

Up to six keys per state, in priority order (the key that moves the task on first), plus `?` for the rest. The help overlay still lists every key.

Done when: footer snapshots at 80 and 100 columns for Review, Failed, Proposed and a question, none clipped.

## Current code

- `KEYS` (`src/tui.rs:48`) has 19 entries; `draw` filters them per selection (~L1290–1322). At 100 columns the `main_screen` snapshot clips after `c` (~L2983).
